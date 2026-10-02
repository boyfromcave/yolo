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
use crate::rpc::RpcClient;
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
    /// Submits rejected locally (malformed, high-hash) before the miner is disconnected.
    pub max_bad_submits: u32,
    /// `mining.authorize` attempts per IP per minute.
    pub authorize_per_minute: u32,
    /// How long a `validateaddress` answer is reused.
    pub address_cache_ttl: Duration,
    /// Compare each submit's block hash with the target before `submitblock` (audit H-3).
    /// Off only in the wire-replay tests, whose recorded submits belong to another coinbase.
    pub check_pow: bool,
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
        }
    }
}

/// A `validateaddress` answer: the scriptPubKey, or None for an invalid/shielded address.
type AddressAnswer = Option<Vec<u8>>;
const ADDRESS_CACHE_MAX: usize = 4096;
const AUTH_RATE_MAX_IPS: usize = 4096;

pub struct Inner {
    pub template: Option<BlockTemplate>,
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
    pub last_verdict: String,
    pub accepted: u64,
    pub rejected: u64,
    /// The `previousblockhash` (wire order, as `Work` carries it) of the last job the node
    /// accepted a block for: a template still on that parent is stale (the node has moved on)
    /// and no work is built from it until the poller brings the next one — otherwise a fast
    /// solver re-solves the old job and every submit comes back `inconclusive`.
    pub accepted_parent: Option<String>,
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
    pub rpc: RpcClient,
    pub policy: Policy,
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
}

pub type State = Arc<Shared>;

impl Shared {
    pub fn new(rpc: RpcClient, policy: Policy, equihash: Equihash, password: Option<String>, limits: Limits) -> (State, watch::Receiver<u64>) {
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
                last_verdict: String::new(),
                accepted: 0,
                rejected: 0,
                accepted_parent: None,
            }),
            generation_tx,
            refresh: Notify::new(),
            rpc,
            policy,
            equihash,
            password,
            started: Instant::now(),
            connection_permits: Arc::new(Semaphore::new(limits.max_connections)),
            submit_permits: Semaphore::new(limits.submits_in_flight),
            limits,
            per_ip: Mutex::new(HashMap::new()),
            authorize_rate: Mutex::new(HashMap::new()),
            address_cache: Mutex::new(HashMap::new()),
        });
        (shared, rx)
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

    /// A cached `validateaddress` answer for `address`, if still fresh.
    pub fn cached_address(&self, address: &str) -> Option<AddressAnswer> {
        let m = self.address_cache.lock().unwrap_or_else(|e| e.into_inner());
        m.get(address).filter(|(at, _)| at.elapsed() < self.limits.address_cache_ttl).map(|(_, spk)| spk.clone())
    }

    pub fn cache_address(&self, address: &str, script_pubkey: AddressAnswer) {
        let mut m = self.address_cache.lock().unwrap_or_else(|e| e.into_inner());
        if m.len() >= ADDRESS_CACHE_MAX {
            let ttl = self.limits.address_cache_ttl;
            m.retain(|_, (at, _)| at.elapsed() < ttl);
            if m.len() >= ADDRESS_CACHE_MAX {
                return;
            }
        }
        m.insert(address.to_string(), (Instant::now(), script_pubkey));
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
        let g = self.lock();
        serde_json::json!({
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
            "connections": g.connections,
            "tag": g.last_tag_kind,
            "lastSubmitVerdict": g.last_verdict,
            "accepted": g.accepted,
            "rejected": g.rejected,
            "uptimeSeconds": self.started.elapsed().as_secs(),
        })
    }
}
