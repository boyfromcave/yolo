// Copyright (c) 2026 The Ycash developers
// Distributed under the MIT software license, see the accompanying
// file LICENSE or https://www.opensource.org/licenses/mit-license.php .

//! Shared server state: the current template, the counters `/status` reports, and the
//! generation number that tells clients to fetch new work.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{watch, Notify, Semaphore};

use crate::equihash::Equihash;
use crate::pairs::{Pairs, PayoutKind};
use crate::rpc::{CoinbaseSource, RpcClient};
use crate::template::{BlockTemplate, ChangeKey};
use crate::work::Policy;

/// Resource limits on the stratum side (audit H-2, H-3, H-4). The CLI sets the two
/// connection caps; the rest are fixed and overridden only by tests.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Sockets accepted at once, authorized or not (`--max-connections`).
    pub max_connections: usize,
    /// Sockets accepted at once from one IP (`--max-per-ip`).
    pub max_per_ip: usize,
    /// A socket that has not completed `mining.authorize` by then is closed.
    pub auth_timeout: Duration,
    /// An authorized miner silent for this many keepalive periods is closed.
    pub idle_keepalives: u32,
    /// `submitblock` calls in flight across all miners; one per miner by construction.
    pub submits_in_flight: usize,
    /// Submits refused since the last accepted block (malformed, wrong ntime, duplicate,
    /// bad solution, rejected by the node; not stale or high-hash) before the miner is disconnected.
    pub max_bad_submits: u32,
    /// `mining.authorize` attempts per IP per minute that fail or cost a `validateaddress` call
    /// (audit H-4). A successful one answered without the node (the address cached, or
    /// `--payout`) is refunded: a miner that reconnects after a stale share or a dropped link
    /// re-authorizes for free, while password guessing and address floods stay capped.
    pub authorize_per_minute: u32,
    /// How long a `validateaddress` answer is reused.
    pub address_cache_ttl: Duration,
    /// Compare each submit's block hash with the target and verify the Equihash solution of
    /// one that meets it before `submitblock` (audit H-3). Off only in the wire-replay tests,
    /// whose recorded submits belong to another coinbase.
    pub check_pow: bool,
    /// `--share-ratio`: the target sent to miners is this many times easier than the block's
    /// (capped at the all-ones target). 1 = the block target, as the Perl did.
    pub share_ratio: u32,
    /// `--coinbase-concurrency`: `getcoinbasetemplatefor` requests in flight at once (note §6.3).
    pub coinbase_concurrency: usize,
    /// `--max-paired-per-ip`: distinct Sapling and post-quantum payout addresses one source IP
    /// may mine to at once (each costs the node a coinbase per block).
    pub max_paired_per_ip: usize,
    /// While the node answers `getcoinbasetemplatefor` with "Method not found", it is asked
    /// again this often.
    pub coinbase_probe_every: Duration,
    /// A miner that has been sent nothing for this long is sent its job again (keepalive).
    pub keepalive: Duration,
    /// `--min-block-interval`: a block is submitted no sooner than this after its job's time was
    /// read from the node (`crate::pace`). Zero: off, every block goes to the node at once.
    pub min_block_interval: Duration,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_connections: 1024,
            max_per_ip: 64,
            auth_timeout: Duration::from_secs(30),
            idle_keepalives: 3,
            submits_in_flight: 8,
            max_bad_submits: 10,
            authorize_per_minute: 10,
            address_cache_ttl: Duration::from_secs(600),
            check_pow: true,
            share_ratio: 1,
            coinbase_concurrency: 2,
            max_paired_per_ip: 8,
            coinbase_probe_every: Duration::from_secs(60),
            keepalive: crate::stratum::MINER_TIMEOUT,
            min_block_interval: Duration::ZERO,
        }
    }
}

/// What the node said a username's address is: its payout kind, or None (refused).
type AddressAnswer = Option<PayoutKind>;
const ADDRESS_CACHE_MAX: usize = 4096;
const AUTH_RATE_MAX_IPS: usize = 4096;

/// An authorized connection, as `/status` lists it.
#[derive(Debug, Clone)]
pub struct MinerInfo {
    /// The payout address (or, with `--payout`, the fixed one).
    pub address: String,
    /// The worker label (`<address>.<label>`), or with `--payout` the username (cut to 32).
    pub label: Option<String>,
    /// `p2pkh`, `p2sh`, `pq`, `sapling`, or `fixed` under `--payout`.
    pub kind: &'static str,
}

pub struct Inner {
    pub template: Option<Arc<BlockTemplate>>,
    pub key: ChangeKey,
    pub template_at: Option<Instant>,
    pub generation: u64,
    pub node_up: bool,
    /// Authorized miners (an unauthorized socket is a connection, not a miner: H-2).
    pub miners: usize,
    /// Every open stratum socket.
    pub connections: usize,
    pub clients_seen: u64,
    pub last_tag_kind: &'static str,
    /// The last template's tag version (1, or 2 from beta4) and its payee scheme (`yed_gettag`'s
    /// `payoutScheme`), for `/status`.
    pub last_tag: Option<(u8, &'static str)>,
    pub last_verdict: String,
    pub accepted: u64,
    pub rejected: u64,
    /// Submits for a job the chain has moved past (another parent, or an unknown job id): answered
    /// `[21, …]`, never forwarded, not counted in `rejected` nor against the miner.
    pub stale: u64,
    /// Submits whose hash is above the share target (`high-hash`, answered `[23, …]`): a GPU
    /// miner sends some (miniZ about one per five or six shares, testnet-yb-fast 2026-11-13),
    /// so like a stale submit it is counted here, not in `rejected` nor against the miner. It
    /// costs the pool one SHA256d, the same as a share; never forwarded.
    pub high_hash: u64,
    /// Submits that met the share target but not the block's (`--share-ratio`); never forwarded.
    pub shares: u64,
    /// The `previousblockhash` (wire order, as `Work` carries it) of the last job the node
    /// accepted a block for: a template still on that parent is stale (the node has moved on)
    /// and no work is built from it until the poller brings the next one — otherwise a fast
    /// solver re-solves the old job and every submit comes back `inconclusive`.
    pub accepted_parent: Option<String>,
    /// Block pacing (`--min-block-interval`): the held block and its counters.
    pub pacer: crate::pace::Pacer,
}

impl Inner {
    /// True while the current template is one the node has already built on.
    pub fn template_stale(&self) -> bool {
        match (&self.template, &self.accepted_parent) {
            (Some(t), Some(parent)) => crate::codec::reverse_hex(&t.previousblockhash) == *parent,
            _ => false,
        }
    }
}

pub struct Shared {
    pub inner: Mutex<Inner>,
    pub generation_tx: watch::Sender<u64>,
    /// Wakes the poller before its next 1 s tick (after an accepted `submitblock`).
    pub refresh: Notify,
    /// Wakes the release task when a block is held (`crate::pace::run`).
    pub pace_wake: Notify,
    /// A reason to stop the pool (`Shared::fail`): `Bound::serve` returns it as its error.
    pub fatal: watch::Sender<Option<String>>,
    pub rpc: RpcClient,
    pub policy: Policy,
    /// Where paired coinbases come from (the node's `getcoinbasetemplatefor`); None when the node
    /// does not serve it or a `--payout` makes every job a swap job: Sapling and post-quantum
    /// usernames are then refused.
    pub coinbase: Option<Arc<dyn CoinbaseSource>>,
    /// The pair book (`crate::pairs`).
    pub pairs: Pairs,
    pub equihash: Equihash,
    pub password: Option<String>,
    pub started: Instant,
    pub limits: Limits,
    /// One permit per open stratum socket (`Limits::max_connections`).
    pub connection_permits: Arc<Semaphore>,
    /// One permit per `submitblock` in flight (`Limits::submits_in_flight`).
    pub submit_permits: Semaphore,
    per_ip: Mutex<HashMap<IpAddr, usize>>,
    authorize_rate: Mutex<HashMap<IpAddr, (Instant, u32)>>,
    address_cache: Mutex<HashMap<String, (Instant, AddressAnswer)>>,
    miner_list: Mutex<HashMap<u64, MinerInfo>>,
}

pub type State = Arc<Shared>;

impl Shared {
    pub fn new(rpc: RpcClient, policy: Policy, coinbase: Option<Arc<dyn CoinbaseSource>>, equihash: Equihash, password: Option<String>, limits: Limits) -> (State, watch::Receiver<u64>) {
        let (generation_tx, rx) = watch::channel(0);
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                template: None,
                key: ChangeKey::default(),
                template_at: None,
                generation: 0,
                node_up: false,
                miners: 0,
                connections: 0,
                clients_seen: 0,
                last_tag_kind: "none",
                last_tag: None,
                last_verdict: String::new(),
                accepted: 0,
                rejected: 0,
                stale: 0,
                high_hash: 0,
                shares: 0,
                accepted_parent: None,
                pacer: crate::pace::Pacer::new(limits.min_block_interval),
            }),
            generation_tx,
            refresh: Notify::new(),
            pace_wake: Notify::new(),
            fatal: watch::channel(None).0,
            rpc,
            policy,
            coinbase,
            pairs: Pairs::new(limits.coinbase_concurrency, limits.max_paired_per_ip, limits.coinbase_probe_every),
            equihash,
            password,
            started: Instant::now(),
            connection_permits: Arc::new(Semaphore::new(limits.max_connections)),
            submit_permits: Semaphore::new(limits.submits_in_flight),
            limits,
            per_ip: Mutex::new(HashMap::new()),
            authorize_rate: Mutex::new(HashMap::new()),
            address_cache: Mutex::new(HashMap::new()),
            miner_list: Mutex::new(HashMap::new()),
        });
        (shared, rx)
    }

    /// Stops the pool: `Bound::serve` ends with this error (the first one given).
    pub fn fail(&self, why: String) {
        self.fatal.send_if_modified(|f| {
            if f.is_none() {
                *f = Some(why);
                true
            } else {
                false
            }
        });
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Counts a new socket from `ip`; false when the IP is at `Limits::max_per_ip`.
    pub fn ip_connect(&self, ip: IpAddr) -> bool {
        let mut m = self.per_ip.lock().unwrap_or_else(|e| e.into_inner());
        let n = m.entry(ip).or_insert(0);
        if *n >= self.limits.max_per_ip {
            return false;
        }
        *n += 1;
        true
    }

    pub fn ip_disconnect(&self, ip: IpAddr) {
        let mut m = self.per_ip.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = m.get_mut(&ip) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                m.remove(&ip);
            }
        }
    }

    /// One `mining.authorize` attempt from `ip`; false once the per-minute budget is spent.
    /// Charged before the attempt so concurrent sockets from one IP cannot overrun it; a cheap
    /// success is given back with `authorize_refund`.
    pub fn authorize_allowed(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut m = self.authorize_rate.lock().unwrap_or_else(|e| e.into_inner());
        if m.len() >= AUTH_RATE_MAX_IPS {
            m.retain(|_, (since, _)| now.duration_since(*since) < Duration::from_secs(60));
        }
        let e = m.entry(ip).or_insert((now, 0));
        if now.duration_since(e.0) >= Duration::from_secs(60) {
            *e = (now, 0);
        }
        if e.1 >= self.limits.authorize_per_minute {
            return false;
        }
        e.1 += 1;
        true
    }

    /// Gives back the attempt `authorize_allowed` charged: the authorization succeeded without a
    /// `validateaddress` call.
    pub fn authorize_refund(&self, ip: IpAddr) {
        let mut m = self.authorize_rate.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = m.get_mut(&ip) {
            e.1 = e.1.saturating_sub(1);
        }
    }

    /// A cached `validateaddress` answer for `address`, if still fresh.
    pub fn cached_address(&self, address: &str) -> Option<AddressAnswer> {
        let m = self.address_cache.lock().unwrap_or_else(|e| e.into_inner());
        m.get(address).filter(|(at, _)| at.elapsed() < self.limits.address_cache_ttl).map(|(_, spk)| spk.clone())
    }

    pub fn cache_address(&self, address: &str, kind: AddressAnswer) {
        let mut m = self.address_cache.lock().unwrap_or_else(|e| e.into_inner());
        if m.len() >= ADDRESS_CACHE_MAX {
            let ttl = self.limits.address_cache_ttl;
            m.retain(|_, (at, _)| at.elapsed() < ttl);
            if m.len() >= ADDRESS_CACHE_MAX {
                return;
            }
        }
        m.insert(address.to_string(), (Instant::now(), kind));
    }

    /// Lists an authorized connection on `/status`.
    pub fn miner_authorized(&self, index: u64, info: MinerInfo) {
        self.miner_list.lock().unwrap_or_else(|e| e.into_inner()).insert(index, info);
    }

    pub fn miner_gone(&self, index: u64) {
        self.miner_list.lock().unwrap_or_else(|e| e.into_inner()).remove(&index);
    }

    /// Records an accepted block and asks the poller for the next template now.
    pub fn block_accepted(&self, parent: &str) {
        let mut g = self.lock();
        g.accepted += 1;
        g.last_verdict = "accepted".into();
        g.accepted_parent = Some(parent.to_string());
        drop(g);
        self.refresh.notify_one();
    }

    /// The `/status` document.
    pub fn status_json(&self) -> serde_json::Value {
        // The book after the state, never both at once the other way round.
        let tip = self.lock().template.as_ref().map(|t| t.previousblockhash.clone());
        let pairs = self.pairs.status(tip.as_deref());
        let (by_kind, list) = {
            let m = self.miner_list.lock().unwrap_or_else(|e| e.into_inner());
            let mut by_kind = serde_json::Map::new();
            for k in ["p2pkh", "p2sh", "pq", "sapling", "fixed"] {
                by_kind.insert(k.into(), serde_json::json!(m.values().filter(|i| i.kind == k).count()));
            }
            let mut ids: Vec<&u64> = m.keys().collect();
            ids.sort();
            let list: Vec<serde_json::Value> = ids
                .into_iter()
                .map(|id| {
                    let i = &m[id];
                    serde_json::json!({ "id": id, "kind": i.kind, "address": i.address, "label": i.label })
                })
                .collect();
            (by_kind, list)
        };
        let g = self.lock();
        let mut status = serde_json::json!({
            "payout": self.policy.payout_label(),
            // The tag's payoutKey is always the node's -yellowbackpayoutaddress (MINER-2),
            // whatever --payout says about the coinbase payee (audit H-7).
            "tagPayoutKey": "node",
            "text": self.policy.text.is_some(),
            "noFlags": self.policy.no_flags,
            "equihash": self.equihash.to_string(),
            "nodeUp": g.node_up,
            "height": g.template.as_ref().map(|t| t.height),
            "templateAgeSeconds": g.template_at.map(|t| t.elapsed().as_secs_f64().round()),
            "miners": g.miners,
            "minersByKind": by_kind,
            "minerList": list,
            "coinbaseCall": self.coinbase.is_some() && self.pairs.available(),
            "pairs": pairs,
            "connections": g.connections,
            "tag": g.last_tag_kind,
            "tagVersion": g.last_tag.map(|t| t.0),
            "tagPayoutScheme": g.last_tag.map(|t| t.1),
            "lastSubmitVerdict": g.last_verdict,
            "accepted": g.accepted,
            "rejected": g.rejected,
            "stale": g.stale,
            "highHash": g.high_hash,
            "shares": g.shares,
            "shareRatio": self.limits.share_ratio,
            "uptimeSeconds": self.started.elapsed().as_secs(),
            "minBlockInterval": g.pacer.interval().as_secs(),
        });
        if g.pacer.enabled() {
            // Only with --min-block-interval: without it the document is the one it always was,
            // plus the zero above.
            let now = Instant::now();
            let o = status.as_object_mut().expect("an object");
            o.insert("nextBlockInSeconds".into(), serde_json::json!(g.pacer.next_block_in(now).as_secs_f64().ceil()));
            o.insert("held".into(), g.pacer.held_json(now));
            o.insert("heldStale".into(), serde_json::json!(g.pacer.held_stale));
            o.insert("paceCandidates".into(), serde_json::json!(g.pacer.candidates_total));
        }
        status
    }
}
