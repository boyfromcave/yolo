// Copyright (c) 2026 The Ycash developers
// Distributed under the MIT software license, see the accompanying
// file LICENSE or https://www.opensource.org/licenses/mit-license.php .

//! Paired jobs: per-miner Sapling and post-quantum payouts (beta3 item 3,
//! `docs/plans/beta3/getcoinbasetemplatefor.md` §8).
//!
//! A transparent (P2PKH, P2SH) username stays on the swap path: its script is written into
//! `vout[0]` of the node's own coinbase. A Sapling payout has no script, and a post-quantum one is
//! paired too (decision D-6): for the template yolo holds, the node builds the miner's coinbase
//! with `getcoinbasetemplatefor(address, {previousblockhash, txids})`, and the job is the **pair**
//! of that template's transaction set and that coinbase, issued together. yolo never asks for an
//! amount and never alters one: the node derives it (subsidy plus the fees of exactly the named
//! transactions, less the fund share the node's own coinbase would carry) and yolo checks every
//! answer against the stock template before it uses it (§8.3).
//!
//! The book keeps one pair per payout address (connections that share an address share it). A
//! pair is replaced on a new block at once (the old one is void, and no job is issued until the
//! new one arrives), on a new quote tag at once, and on a changed transaction set no more often
//! than every [`TX_REFRESH`]; meanwhile a miner keeps its valid current pair. Requests run on
//! their own threads, at most `--coinbase-concurrency` at once, so neither the template poll nor
//! `submitblock` ever waits behind a proof.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{watch, Notify, Semaphore};
use tracing::{debug, error, info, warn};

use crate::codec::dsha256;
use crate::rpc::{CoinbaseFor, RpcClient, RpcError, ValidateAddress, ZValidateAddress, RPC_METHOD_NOT_FOUND};
use crate::state::State;
use crate::tag::height_push;
use crate::template::BlockTemplate;
use crate::tx::Coinbase;

/// A changed transaction set alone renews a pair no more often than this (the node itself
/// rebuilds its template for mempool changes no more often than every 5 s).
pub const TX_REFRESH: Duration = Duration::from_secs(5);
/// A request that failed for a template is retried for the same template after this long.
pub const RETRY_AFTER: Duration = Duration::from_secs(5);
/// The longest worker label (`<address>.<label>`, decision D-5).
pub const MAX_LABEL: usize = 32;

/// How a username is paid, from the node's `validateaddress` / `z_validateaddress`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayoutKind {
    /// Transparent P2PKH: the swap path, with this scriptPubKey.
    P2pkh(Vec<u8>),
    /// Transparent P2SH: the swap path.
    P2sh(Vec<u8>),
    /// Post-quantum plain YEC (`<key hash> <scheme> OP_CHECKPQSIG`): the paired path (D-6); the
    /// script is kept to check that the returned coinbase pays it.
    Pq(Vec<u8>),
    /// Sapling: the paired path.
    Sapling,
}

impl PayoutKind {
    /// The name `/status` and `getcoinbasetemplatefor`'s `addresstype` use.
    pub fn name(&self) -> &'static str {
        match self {
            PayoutKind::P2pkh(_) => "p2pkh",
            PayoutKind::P2sh(_) => "p2sh",
            PayoutKind::Pq(_) => "pq",
            PayoutKind::Sapling => "sapling",
        }
    }

    /// The script the swap path writes into `vout[0]`; None for a paired kind.
    pub fn swap_script(&self) -> Option<&[u8]> {
        match self {
            PayoutKind::P2pkh(s) | PayoutKind::P2sh(s) => Some(s),
            _ => None,
        }
    }

    pub fn is_paired(&self) -> bool {
        self.swap_script().is_none()
    }

    /// From `validateaddress` (a valid answer with a scriptPubKey) and, when that says invalid,
    /// `z_validateaddress`. None: refused (`Invalid address`, as today).
    pub fn classify(v: &ValidateAddress, z: Option<&ZValidateAddress>) -> Option<PayoutKind> {
        if v.isvalid {
            let spk = hex::decode(v.script_pubkey.as_deref()?).ok()?;
            if v.pqscheme.is_some() {
                return Some(PayoutKind::Pq(spk));
            }
            return match spk.as_slice() {
                [0x76, 0xa9, 0x14, .., 0x88, 0xac] if spk.len() == 25 => Some(PayoutKind::P2pkh(spk)),
                [0xa9, 0x14, .., 0x87] if spk.len() == 23 => Some(PayoutKind::P2sh(spk)),
                _ => None,
            };
        }
        let z = z?;
        (z.isvalid && z.address_type() == Some("sapling")).then_some(PayoutKind::Sapling)
    }
}

/// The refusal for a Yugen address (addendum ruling 14: `yqf`/`yqtf`/`yqrf`, reserved
/// `yqk`/`yqtk`/`yqrk`). A Yugen note is not a coinbase output (no consensus path pays a block
/// reward into the Yugen pool), so a pool cannot pay one; the miner is told to use a Sapling,
/// post-quantum or transparent address and shield to Yugen from the wallet. None: not Yugen.
pub fn yugen_refusal(address: &str) -> Option<String> {
    const PREFIXES: [&str; 6] = ["yqtf", "yqrf", "yqtk", "yqrk", "yqf", "yqk"];
    PREFIXES.iter().any(|p| address.starts_with(p)).then(|| {
        "Invalid address: a Yugen address (yq...) cannot receive a block reward; mine to a Sapling, post-quantum (sqf/sqs) or transparent address and move the coins to Yugen from your wallet"
            .to_string()
    })
}

/// Asks the node what `address` is: `validateaddress`, then `z_validateaddress` for an address the
/// first does not know (a Sapling address is invalid to `validateaddress`).
pub fn classify_address(rpc: &RpcClient, address: &str) -> Result<Option<PayoutKind>, RpcError> {
    let v = rpc.validateaddress(address)?;
    if v.isvalid {
        return Ok(PayoutKind::classify(&v, None));
    }
    match rpc.z_validateaddress(address) {
        Ok(z) => Ok(PayoutKind::classify(&v, Some(&z))),
        // `validateaddress` has already said invalid; a node that cannot answer the second
        // question about it has said all there is (an unreachable node stays an error)
        Err(RpcError::Node { .. } | RpcError::Protocol(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `<address>` or `<address>.<label>` (D-5): the part before the first `.` is the address (no
/// address of the three kinds contains one); the label is 1 to 32 characters of
/// `[A-Za-z0-9_-]`, shown in the log and `/status` only.
pub fn split_username(username: &str) -> Result<(&str, Option<&str>), String> {
    let Some((address, label)) = username.split_once('.') else { return Ok((username, None)) };
    if label.is_empty() || label.len() > MAX_LABEL || !label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        return Err(format!("the worker label must be 1 to {} characters of A-Z, a-z, 0-9, _ and -", MAX_LABEL));
    }
    Ok((address, Some(label)))
}

/// What a pair is built for: the template's `previousblockhash` and transaction ids (the
/// reference the node is asked with) and its `coinbaseaux.flags` (a new quote renews the pair).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateRef {
    pub previousblockhash: String,
    pub txids: Vec<String>,
    pub flags: String,
}

impl TemplateRef {
    pub fn of(t: &BlockTemplate) -> TemplateRef {
        TemplateRef { previousblockhash: t.previousblockhash.to_ascii_lowercase(), txids: t.txids(), flags: t.coinbaseaux.flags.to_ascii_lowercase() }
    }
}

/// An answer that passed every check: the coinbase bytes and the tag bytes to use for its job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub coinbase: Vec<u8>,
    pub flags: Vec<u8>,
}

/// §8.3: every check an answer must pass before a job is built from it. `t` is the template the
/// request named, `txids` exactly the ids sent, `kind` the miner's payout kind.
pub fn check_answer(t: &BlockTemplate, kind: &PayoutKind, txids: &[String], a: &CoinbaseFor) -> Result<Checked, String> {
    if !a.previousblockhash.eq_ignore_ascii_case(&t.previousblockhash) {
        return Err(format!("previousblockhash {} is not the template's {}", a.previousblockhash, t.previousblockhash));
    }
    if a.height != t.height {
        return Err(format!("height {} is not the template's {}", a.height, t.height));
    }
    if a.txcount != txids.len() {
        return Err(format!("txcount {} but {} transaction ids were sent", a.txcount, txids.len()));
    }
    let fees = t.fee_total()?;
    if a.fees != fees {
        return Err(format!("fees {} but the template's transactions pay {}", a.fees, fees));
    }
    if a.addresstype != kind.name() {
        return Err(format!("addresstype {} but the miner's address is {}", a.addresstype, kind.name()));
    }
    let coinbase = hex::decode(&a.data).map_err(|e| format!("data is not hex: {}", e))?;
    let cb = Coinbase::parse(&coinbase).map_err(|e| format!("data: {}", e))?;
    let mut txid = dsha256(&coinbase);
    txid.reverse();
    if !a.hash.eq_ignore_ascii_case(&hex::encode(txid)) {
        return Err(format!("hash {} is not the coinbase's {}", a.hash, hex::encode(txid)));
    }
    if !cb.script_sig.starts_with(&height_push(t.height)) {
        return Err(format!("the scriptSig does not start with the height push for {}", t.height));
    }
    let flags = hex::decode(&a.flags).map_err(|e| format!("flags is not hex: {}", e))?;
    // The coinbase's signature operations count toward the block's limit with the template's
    // (the size is checked when the job is built: `work::build_paired_work`).
    if let Some(limit) = t.sigoplimit {
        let total = t.sigop_total()?.checked_add(u64::from(a.sigops)).ok_or("the sigop total overflows")?;
        if total > limit {
            return Err(format!("coinbase-does-not-fit: sigops {} (the coinbase's {} and the template's) exceed {}", total, a.sigops, limit));
        }
    }
    // What the coinbase claims equals what the node's own coinbase for this template claims:
    // subsidy plus every fee (both measured equal, note §10 run 1).
    let stock = hex::decode(&t.coinbasetxn.data).map_err(|e| format!("coinbasetxn.data is not hex: {}", e))?;
    let stock_total = Coinbase::parse(&stock).map_err(|e| format!("coinbasetxn.data: {}", e))?.claimed_total().ok_or("the template's coinbase total overflows")?;
    let claimed = cb.claimed_total().ok_or("the coinbase's total overflows")?;
    if claimed != stock_total {
        return Err(format!("the coinbase claims {} but the template's own coinbase claims {}", claimed, stock_total));
    }
    if a.subsidy.checked_add(a.fees) != Some(claimed) {
        return Err(format!("subsidy {} plus fees {} is not the {} the coinbase claims", a.subsidy, a.fees, claimed));
    }
    if claimed.checked_sub(a.fund) != Some(a.amount) {
        return Err(format!("amount {} is not the claimed {} less the fund {}", a.amount, claimed, a.fund));
    }
    // The fund output is the stock coinbase's (note §8.3, decision D-2): the node's own coinbase
    // pays a transparent miner at vout[0], every further output is the fund share, and the
    // returned coinbase carries exactly those outputs besides the miner's payout.
    let stock_cb = Coinbase::parse(&stock).map_err(|e| format!("coinbasetxn.data: {}", e))?;
    if stock_cb.sapling_outputs != 0 || stock_cb.vout.is_empty() {
        return Err("the template's own coinbase does not pay a transparent address at vout[0]".into());
    }
    let stock_fund = stock_cb.vout[1..].iter().try_fold(0u64, |s, o| s.checked_add(o.value)).ok_or("the template's fund outputs overflow")?;
    if a.fund != stock_fund {
        return Err(format!("fund {} but the template's own coinbase carries {}", a.fund, stock_fund));
    }
    match (a.fundrule.as_str(), a.fund) {
        ("none", 0) => {}
        ("mandated" | "optional", f) if f > 0 => {}
        (rule, f) => return Err(format!("fundrule {} does not fit fund {}", rule, f)),
    }
    for o in &stock_cb.vout[1..] {
        if !cb.vout.iter().any(|v| v == o) {
            return Err(format!("the fund output of {} zat to {} is missing", o.value, hex::encode(&o.script_pubkey)));
        }
    }
    let transparent = cb.transparent_total().ok_or("the coinbase's outputs overflow")?;
    match kind {
        PayoutKind::Sapling => {
            if cb.sapling_outputs != 1 || cb.value_balance.checked_neg() != i64::try_from(a.amount).ok() {
                return Err(format!("a Sapling payout must be one Sapling output of the amount {} (has {} outputs, valueBalance {})", a.amount, cb.sapling_outputs, cb.value_balance));
            }
            if transparent != a.fund {
                return Err(format!("the transparent outputs total {}, not the fund {}", transparent, a.fund));
            }
        }
        PayoutKind::Pq(spk) | PayoutKind::P2pkh(spk) | PayoutKind::P2sh(spk) => {
            if cb.sapling_outputs != 0 || cb.value_balance != 0 {
                return Err("a transparent or post-quantum payout carries no shielded part".into());
            }
            if !cb.vout.iter().any(|o| o.script_pubkey == *spk && o.value == a.amount) {
                return Err(format!("no output pays the amount {} to the miner's script", a.amount));
            }
            if transparent.checked_sub(a.amount) != Some(a.fund) {
                return Err(format!("the outputs besides the payout total {}, not the fund {}", transparent.saturating_sub(a.amount), a.fund));
            }
        }
    }
    Ok(Checked { coinbase, flags })
}

/// One template's transaction set and one coinbase built for exactly that set and one address.
/// The job is always built from `template`, never from a later one.
#[derive(Debug)]
pub struct Pair {
    pub template: Arc<BlockTemplate>,
    pub reference: TemplateRef,
    pub coinbase: Vec<u8>,
    pub flags: Vec<u8>,
    pub amount: u64,
    /// When the pair was made: the oldest pair is renewed first (no address waits behind the
    /// same others every time).
    pub built_at: Instant,
    /// When the pool read `template` from the node (its `curtime` is the job's header time):
    /// block pacing counts from here, not from `built_at`, so the node's time to build the
    /// coinbase is not added to a paired miner's hold.
    pub read_at: Instant,
}

struct Entry {
    kind: PayoutKind,
    subscribers: usize,
    /// When the first connection for the address authorized (orders addresses that have no
    /// pair yet: the longest waiting first).
    subscribed_at: Instant,
    current: Option<Arc<Pair>>,
    /// Bumped whenever `current` is replaced (clients compare it with the job they sent).
    version: u64,
    in_flight: bool,
    last_request: Option<Instant>,
    /// The template the last request failed for, and when (logged once per address and
    /// template; retried after `RETRY_AFTER`).
    failed: Option<(TemplateRef, Instant)>,
}

#[derive(Debug, Default, Clone)]
struct Stats {
    requests: u64,
    failures: u64,
    last_round_trip: Option<Duration>,
}

/// The pair book: one entry per paired payout address with at least one connection.
pub struct Pairs {
    entries: Mutex<HashMap<String, Entry>>,
    /// The paired addresses each source IP has connections for, with their connection counts
    /// (never locked together with `entries`).
    per_ip: Mutex<HashMap<IpAddr, HashMap<String, usize>>>,
    /// `--max-paired-per-ip`: distinct paired addresses one IP may hold at once.
    max_per_ip: usize,
    /// Whether the node serves `getcoinbasetemplatefor`. Cleared by a "Method not found" answer
    /// to any request; while it is clear, paired usernames are refused and the node is
    /// re-probed every `probe_every`.
    available: AtomicBool,
    probe_every: Duration,
    last_probe: Mutex<Option<Instant>>,
    permits: Arc<Semaphore>,
    concurrency: usize,
    /// Wakes the refresher (a new template, a new subscriber, a finished request).
    pub wake: Notify,
    version_tx: watch::Sender<u64>,
    stats: Mutex<Stats>,
}

impl Pairs {
    pub fn new(concurrency: usize, max_per_ip: usize, probe_every: Duration) -> Pairs {
        let concurrency = concurrency.max(1);
        Pairs {
            entries: Mutex::new(HashMap::new()),
            per_ip: Mutex::new(HashMap::new()),
            max_per_ip,
            available: AtomicBool::new(true),
            probe_every,
            last_probe: Mutex::new(None),
            permits: Arc::new(Semaphore::new(concurrency)),
            concurrency,
            wake: Notify::new(),
            version_tx: watch::channel(0).0,
            stats: Mutex::new(Stats::default()),
        }
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Notified whenever any pair is replaced.
    pub fn subscribe_versions(&self) -> watch::Receiver<u64> {
        self.version_tx.subscribe()
    }

    /// A connection from `ip` mining to `address` (a paired kind) has authorized. Refused when
    /// `ip` already holds connections for `--max-paired-per-ip` other paired addresses: each
    /// address costs the node a coinbase (a proof, for Sapling) on every block, so one IP must
    /// not crowd out everyone else's.
    pub fn subscribe(&self, address: &str, kind: PayoutKind, ip: IpAddr) -> Result<(), String> {
        {
            let mut per_ip = self.per_ip.lock().unwrap_or_else(|e| e.into_inner());
            let held = per_ip.entry(ip).or_default();
            if !held.contains_key(address) && held.len() >= self.max_per_ip {
                if held.is_empty() {
                    per_ip.remove(&ip);
                }
                return Err(format!("Too many Sapling and post-quantum payout addresses from this IP (at most {})", self.max_per_ip));
            }
            *held.entry(address.to_string()).or_insert(0) += 1;
        }
        let now = Instant::now();
        self.entries()
            .entry(address.to_string())
            .or_insert_with(|| Entry { kind, subscribers: 0, subscribed_at: now, current: None, version: 0, in_flight: false, last_request: None, failed: None })
            .subscribers += 1;
        self.wake.notify_one();
        Ok(())
    }

    /// That connection has gone; the entry goes with the last one.
    pub fn unsubscribe(&self, address: &str, ip: IpAddr) {
        {
            let mut per_ip = self.per_ip.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(held) = per_ip.get_mut(&ip) {
                if let Some(n) = held.get_mut(address) {
                    *n = n.saturating_sub(1);
                    if *n == 0 {
                        held.remove(address);
                    }
                }
                if held.is_empty() {
                    per_ip.remove(&ip);
                }
            }
        }
        let mut m = self.entries();
        if let Some(e) = m.get_mut(address) {
            e.subscribers = e.subscribers.saturating_sub(1);
            if e.subscribers == 0 {
                m.remove(address);
            }
        }
    }

    /// The current pair for `address` and its version.
    pub fn current(&self, address: &str) -> Option<(u64, Arc<Pair>)> {
        let m = self.entries();
        let e = m.get(address)?;
        e.current.as_ref().map(|p| (e.version, p.clone()))
    }

    pub fn concurrency(&self) -> usize {
        self.concurrency
    }

    /// Whether the node serves `getcoinbasetemplatefor` (as far as yolo knows).
    pub fn available(&self) -> bool {
        self.available.load(Ordering::SeqCst)
    }

    /// Records what a probe (or the startup check) found. A change wakes every paired miner,
    /// which leaves when the call is gone.
    pub fn set_available(&self, yes: bool) {
        *self.last_probe.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        if self.available.swap(yes, Ordering::SeqCst) != yes {
            if yes {
                info!("the node serves getcoinbasetemplatefor: Sapling and post-quantum usernames are accepted again");
            } else {
                warn!(
                    "the node does not serve getcoinbasetemplatefor (Method not found): Sapling and post-quantum usernames are refused until it does (asked again every {} s)",
                    self.probe_every.as_secs()
                );
            }
            self.version_tx.send_modify(|v| *v += 1);
        }
    }

    /// True (and the probe clock restarted) when the call is missing and the last probe is
    /// `probe_every` old.
    fn probe_due(&self) -> bool {
        if self.available() {
            return false;
        }
        let mut last = self.last_probe.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|at| at.elapsed() < self.probe_every) {
            return false;
        }
        *last = Some(Instant::now());
        true
    }

    /// How many distinct paired addresses `ip` holds connections for.
    pub fn paired_per_ip(&self, ip: IpAddr) -> usize {
        self.per_ip.lock().unwrap_or_else(|e| e.into_inner()).get(&ip).map_or(0, HashMap::len)
    }

    /// `/status`: addresses without a pair for the tip `previousblockhash`, the requests in
    /// flight, and the last round trip.
    pub fn status(&self, previousblockhash: Option<&str>) -> serde_json::Value {
        let (waiting, in_flight, addresses) = {
            let m = self.entries();
            let waiting = m
                .values()
                .filter(|e| e.in_flight || e.current.as_ref().is_none_or(|p| Some(p.reference.previousblockhash.as_str()) != previousblockhash.map(str::to_ascii_lowercase).as_deref()))
                .count();
            (waiting, m.values().filter(|e| e.in_flight).count(), m.len())
        };
        let s = self.stats.lock().unwrap_or_else(|e| e.into_inner()).clone();
        serde_json::json!({
            "addresses": addresses,
            "waiting": waiting,
            "inFlight": in_flight,
            "concurrency": self.concurrency,
            "requests": s.requests,
            "failures": s.failures,
            "lastRoundTripMs": s.last_round_trip.map(|d| d.as_millis() as u64),
        })
    }
}

/// Why an entry needs a new pair for `r`, if it does; lower sorts first.
fn need(e: &Entry, r: &TemplateRef, now: Instant) -> Option<u8> {
    if e.subscribers == 0 || e.in_flight {
        return None;
    }
    if let Some((failed, at)) = &e.failed {
        if failed == r && now.duration_since(*at) < RETRY_AFTER {
            return None;
        }
    }
    match &e.current {
        None => Some(0),
        Some(p) if p.reference.previousblockhash != r.previousblockhash => Some(0),
        Some(p) if p.reference.flags != r.flags => Some(1),
        Some(p) if p.reference.txids != r.txids && e.last_request.is_none_or(|at| now.duration_since(at) >= TX_REFRESH) => Some(2),
        _ => None,
    }
}

/// The addresses that need a new pair for `r`, in the order they are served: new blocks first,
/// then new quotes, then changed transaction sets; within each, the address whose current pair
/// is the oldest (one without a pair before any with one, the longest subscribed first). The
/// address string never decides, so under load the same addresses do not always lose.
fn order(m: &HashMap<String, Entry>, r: &TemplateRef, now: Instant) -> Vec<String> {
    let mut wanted: Vec<(u8, (bool, Instant), &String)> = m
        .iter()
        .filter_map(|(a, e)| {
            let age = match &e.current {
                None => (false, e.subscribed_at),
                Some(p) => (true, p.built_at),
            };
            need(e, r, now).map(|p| (p, age, a))
        })
        .collect();
    wanted.sort();
    wanted.into_iter().map(|(_, _, a)| a.clone()).collect()
}

/// The refresher: on every wake-up (and at least once a second) starts the requests the current
/// template calls for, new blocks first, within the concurrency cap.
pub async fn run(state: State) {
    loop {
        tokio::select! {
            _ = state.pairs.wake.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
        probe(&state);
        refresh(&state);
    }
}

/// While the node is known not to serve `getcoinbasetemplatefor`, asks it again every
/// `probe_every`.
fn probe(state: &State) {
    if !state.pairs.probe_due() {
        return;
    }
    let Some(source) = state.coinbase.clone() else { return };
    let st = state.clone();
    tokio::task::spawn_blocking(move || match source.has_call() {
        Ok(found) => {
            if found {
                st.pairs.set_available(true);
                st.pairs.wake.notify_one();
            }
        }
        Err(e) => debug!("getcoinbasetemplatefor probe: {}", e),
    });
}

/// One scan of the book against the current template.
pub fn refresh(state: &State) {
    let Some(source) = state.coinbase.clone() else { return };
    if !state.pairs.available() {
        return;
    }
    let (template, read_at) = {
        let g = state.lock();
        if g.template_stale() {
            return;
        }
        match &g.template {
            Some(t) => (t.clone(), g.template_at.unwrap_or_else(Instant::now)),
            None => return,
        }
    };
    let r = TemplateRef::of(&template);
    let now = Instant::now();
    let mut started = Vec::new();
    {
        let mut m = state.pairs.entries();
        for address in order(&m, &r, now) {
            let Ok(permit) = state.pairs.permits.clone().try_acquire_owned() else { break };
            let e = m.get_mut(&address).expect("listed above");
            e.in_flight = true;
            e.last_request = Some(now);
            started.push((address, e.kind.clone(), permit));
        }
    }
    for (address, kind, permit) in started {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let (src, a, prev, txids) = (source.clone(), address.clone(), r.previousblockhash.clone(), r.txids.clone());
        let spawned = std::thread::Builder::new().name("yolo-coinbase".into()).spawn(move || {
            let at = Instant::now();
            let answer = src.coinbase_for(&a, &prev, &txids);
            let _ = tx.send((answer, at.elapsed()));
        });
        let st = state.clone();
        let (t, r) = (template.clone(), r.clone());
        if let Err(e) = spawned {
            error!("cannot spawn a coinbase request thread: {}", e);
            finish(&st, &address, &kind, (&t, read_at), &r, Err(RpcError::Protocol(format!("thread: {}", e))), None);
            drop(permit);
            continue;
        }
        tokio::spawn(async move {
            match rx.await {
                Ok((answer, rtt)) => finish(&st, &address, &kind, (&t, read_at), &r, answer, Some(rtt)),
                Err(_) => finish(&st, &address, &kind, (&t, read_at), &r, Err(RpcError::Protocol("the request thread died".into())), None),
            }
            drop(permit);
            st.pairs.wake.notify_one();
        });
    }
}

/// Records the outcome of one request.
fn finish(state: &State, address: &str, kind: &PayoutKind, (t, read_at): (&Arc<BlockTemplate>, Instant), r: &TemplateRef, answer: Result<CoinbaseFor, RpcError>, rtt: Option<Duration>) {
    {
        let mut s = state.pairs.stats.lock().unwrap_or_else(|e| e.into_inner());
        s.requests += 1;
        if rtt.is_some() {
            s.last_round_trip = rtt;
        }
    }
    if matches!(&answer, Err(e) if e.code() == Some(RPC_METHOD_NOT_FOUND)) {
        // The node has no such call (the startup probe could not tell, or it was downgraded):
        // say so once, refuse paired usernames from now on, and ask again later.
        if let Some(e) = state.pairs.entries().get_mut(address) {
            e.in_flight = false;
        }
        state.pairs.set_available(false);
        return;
    }
    let checked = match &answer {
        Ok(a) => check_answer(t, kind, &r.txids, a).map_err(|why| format!("refused answer of getcoinbasetemplatefor: {}", why)),
        Err(e) => Err(e.to_string()),
    };
    // The tip the answer is for must still be the pool's (read before the book: lock order is
    // the state, then the book, never the reverse).
    let tip = state.lock().template.as_ref().map(|c| c.previousblockhash.to_ascii_lowercase());
    let mut m = state.pairs.entries();
    let Some(e) = m.get_mut(address) else { return };
    e.in_flight = false;
    match checked {
        Ok(c) => {
            e.failed = None;
            if tip.as_deref() != Some(r.previousblockhash.as_str()) {
                debug!("coinbase for {} at height {} arrived after the tip moved; dropped", crate::stratum::shown(address), t.height);
                return;
            }
            let amount = answer.as_ref().map(|a| a.amount).unwrap_or(0);
            e.current = Some(Arc::new(Pair { template: t.clone(), reference: r.clone(), coinbase: c.coinbase, flags: c.flags, amount, built_at: Instant::now(), read_at }));
            e.version += 1;
            drop(m);
            info!(
                "pair for {} ({}): height {}, {} txs, amount {} zat{}",
                crate::stratum::shown(address),
                kind.name(),
                t.height,
                r.txids.len(),
                amount,
                rtt.map(|d| format!(", {} ms", d.as_millis())).unwrap_or_default()
            );
            state.pairs.version_tx.send_modify(|v| *v += 1);
        }
        Err(why) => {
            let first = e.failed.as_ref().is_none_or(|(f, _)| f != r);
            e.failed = Some((r.clone(), Instant::now()));
            drop(m);
            state.pairs.stats.lock().unwrap_or_else(|e| e.into_inner()).failures += 1;
            let stale = matches!(&answer, Err(e) if e.is_template_stale());
            if stale {
                // The node has moved on (a new block, or a named transaction left its mempool):
                // fetch a fresh template now; the next one renews the pair.
                debug!("coinbase for {}: {}; refreshing the template", crate::stratum::shown(address), why);
                state.refresh.notify_one();
            } else if first {
                if answer.is_ok() {
                    error!("coinbase for {} at height {}: {}", crate::stratum::shown(address), t.height, why);
                } else {
                    warn!("coinbase for {} at height {}: {}", crate::stratum::shown(address), t.height, why);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template(json: &str) -> BlockTemplate {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn usernames_with_and_without_a_label() {
        assert_eq!(split_username("smabc").unwrap(), ("smabc", None));
        assert_eq!(split_username("smabc.rig1").unwrap(), ("smabc", Some("rig1")));
        assert_eq!(split_username("smabc.rig_1-a").unwrap(), ("smabc", Some("rig_1-a")));
        assert_eq!(split_username(&format!("ys1x.{}", "a".repeat(32))).unwrap().1.map(str::len), Some(32));
        assert!(split_username(&format!("ys1x.{}", "a".repeat(33))).is_err());
        assert!(split_username("smabc.").is_err());
        assert!(split_username("smabc.rig.1").is_err(), "the label is everything after the first dot");
        assert!(split_username("smabc.rig 1").is_err());
        assert!(split_username("smabc.ríg").is_err());
    }

    fn va(isvalid: bool, spk: Option<&str>, pq: Option<i64>) -> ValidateAddress {
        ValidateAddress { isvalid, script_pubkey: spk.map(String::from), ismine: None, pqscheme: pq }
    }

    #[test]
    fn classification() {
        let p2pkh = "76a914b5521b95530df65bec840c03c0e90a126c67625888ac";
        let p2sh = "a914b5521b95530df65bec840c03c0e90a126c67625887";
        let pq = format!("20{}52c2", "11".repeat(32));
        assert_eq!(PayoutKind::classify(&va(true, Some(p2pkh), None), None).unwrap().name(), "p2pkh");
        assert_eq!(PayoutKind::classify(&va(true, Some(p2sh), None), None).unwrap().name(), "p2sh");
        let k = PayoutKind::classify(&va(true, Some(&pq), Some(2)), None).unwrap();
        assert_eq!(k, PayoutKind::Pq(hex::decode(&pq).unwrap()));
        assert!(k.is_paired() && k.swap_script().is_none());
        assert_eq!(PayoutKind::classify(&va(true, Some(p2pkh), None), None).unwrap().swap_script(), Some(&hex::decode(p2pkh).unwrap()[..]));
        // a script of no known shape, and a valid answer without a script, are refused
        assert!(PayoutKind::classify(&va(true, Some(&pq), None), None).is_none());
        assert!(PayoutKind::classify(&va(true, None, None), None).is_none());
        // Sapling: invalid to validateaddress, `sapling` to z_validateaddress (either key name)
        let z = |t: &str, dd: bool| ZValidateAddress { isvalid: true, address_type: (!dd).then(|| t.to_string()), kind: dd.then(|| t.to_string()) };
        assert_eq!(PayoutKind::classify(&va(false, None, None), Some(&z("sapling", false))), Some(PayoutKind::Sapling));
        assert_eq!(PayoutKind::classify(&va(false, None, None), Some(&z("sapling", true))), Some(PayoutKind::Sapling));
        assert!(PayoutKind::classify(&va(false, None, None), Some(&z("sprout", false))).is_none());
        assert!(PayoutKind::classify(&va(false, None, None), Some(&ZValidateAddress { isvalid: false, address_type: None, kind: None })).is_none());
        assert!(PayoutKind::classify(&va(false, None, None), None).is_none());
    }

    /// The answer `getcoinbasetemplatefor` gives for the pool node's template 111 (two
    /// transactions, 489 zatoshi of fees, no fund output): the Sapling coinbase the helper node
    /// built for the same tip and the same transactions (note §8.4's stub).
    fn sapling_answer(t: &BlockTemplate) -> CoinbaseFor {
        let helper = template(include_str!("../tests/vectors/regtest-template-111-sapling.json"));
        CoinbaseFor {
            data: helper.coinbasetxn.data.clone(),
            hash: helper.coinbasetxn.hash.clone().unwrap(),
            height: t.height,
            previousblockhash: t.previousblockhash.clone(),
            addresstype: "sapling".into(),
            subsidy: 625_000_000,
            fund: 0,
            fundrule: "none".into(),
            fees: 489,
            amount: 625_000_489,
            txcount: 2,
            flags: String::new(),
            sigops: 0,
        }
    }

    /// For a post-quantum address: the node's coinbase with the miner's script in place of its own.
    fn pq_answer(t: &BlockTemplate, spk: &[u8]) -> CoinbaseFor {
        let mut cb = Coinbase::parse(&hex::decode(&t.coinbasetxn.data).unwrap()).unwrap();
        cb.vout[0].script_pubkey = spk.to_vec();
        let data = cb.serialize();
        let mut h = dsha256(&data);
        h.reverse();
        CoinbaseFor { data: hex::encode(&data), hash: hex::encode(h), addresstype: "pq".into(), sigops: 1, ..sapling_answer(t) }
    }

    fn pq_script() -> Vec<u8> {
        hex::decode(format!("20{}52c2", "ab".repeat(32))).unwrap()
    }

    #[test]
    fn a_good_answer_passes() {
        let t = template(include_str!("../tests/vectors/regtest-template-111-nofund.json"));
        let a = sapling_answer(&t);
        let c = check_answer(&t, &PayoutKind::Sapling, &t.txids(), &a).unwrap();
        assert_eq!(hex::encode(&c.coinbase), a.data);
        assert!(c.flags.is_empty());
        let pq = pq_script();
        let a = pq_answer(&t, &pq);
        check_answer(&t, &PayoutKind::Pq(pq.clone()), &t.txids(), &a).unwrap();
    }

    /// Each check of §8.3 tripped once.
    #[test]
    fn every_check_trips() {
        let t = template(include_str!("../tests/vectors/regtest-template-111-nofund.json"));
        let ids = t.txids();
        let good = sapling_answer(&t);
        let refused = |a: &CoinbaseFor, kind: &PayoutKind, ids: &[String], what: &str| {
            let e = check_answer(&t, kind, ids, a).unwrap_err();
            assert!(e.contains(what), "expected {:?} in {:?}", what, e);
        };
        let s = PayoutKind::Sapling;
        refused(&CoinbaseFor { previousblockhash: "00".repeat(32), ..good.clone() }, &s, &ids, "previousblockhash");
        refused(&CoinbaseFor { height: 112, ..good.clone() }, &s, &ids, "height");
        refused(&CoinbaseFor { txcount: 1, ..good.clone() }, &s, &ids, "txcount");
        refused(&good, &s, &ids[..1], "txcount");
        refused(&CoinbaseFor { fees: 488, txcount: 2, ..good.clone() }, &s, &ids, "fees");
        refused(&CoinbaseFor { addresstype: "pq".into(), ..good.clone() }, &s, &ids, "addresstype");
        refused(&CoinbaseFor { data: "zz".into(), ..good.clone() }, &s, &ids, "not hex");
        refused(&CoinbaseFor { data: good.data[..good.data.len() - 2].to_string(), ..good.clone() }, &s, &ids, "data:");
        refused(&CoinbaseFor { hash: "00".repeat(32), ..good.clone() }, &s, &ids, "hash");
        refused(&CoinbaseFor { flags: "x".into(), ..good.clone() }, &s, &ids, "flags");
        // a lower amount, or an amount that disagrees with the coinbase, is refused
        refused(&CoinbaseFor { amount: 625_000_000, ..good.clone() }, &s, &ids, "amount");
        refused(&CoinbaseFor { subsidy: 624_999_999, ..good.clone() }, &s, &ids, "subsidy");
        refused(&CoinbaseFor { fund: 1, fundrule: "optional".into(), ..good.clone() }, &s, &ids, "amount");
        refused(&CoinbaseFor { fundrule: "optional".into(), ..good.clone() }, &s, &ids, "fundrule");
        // the coinbase's sigops and the template's together must fit the block's limit (the
        // template's two transactions carry 2 each)
        check_answer(&t, &s, &ids, &CoinbaseFor { sigops: 20_000 - 4, ..good.clone() }).unwrap();
        refused(&CoinbaseFor { sigops: 20_000 - 3, ..good.clone() }, &s, &ids, "coinbase-does-not-fit: sigops 20001");
        let mut tight = t.clone();
        tight.sigoplimit = Some(4);
        assert!(check_answer(&tight, &s, &ids, &CoinbaseFor { sigops: 1, ..good.clone() }).unwrap_err().contains("sigops 5"));
        let mut nosigops = t.clone();
        Arc::make_mut(&mut nosigops.transactions[1]).sigops = None;
        assert!(check_answer(&nosigops, &s, &ids, &good).unwrap_err().contains("no sigop count"));
        // the coinbase must claim what the template's own coinbase claims
        let mut low = t.clone();
        let mut cb = Coinbase::parse(&hex::decode(&low.coinbasetxn.data).unwrap()).unwrap();
        cb.vout[0].value += 1;
        low.coinbasetxn.data = hex::encode(cb.serialize());
        let e = check_answer(&low, &s, &ids, &good).unwrap_err();
        assert!(e.contains("claims"), "{}", e);
        // the template must state every fee
        let mut nofee = t.clone();
        Arc::make_mut(&mut nofee.transactions[0]).fee = None;
        assert!(check_answer(&nofee, &s, &ids, &good).unwrap_err().contains("no fee"));
        // the scriptSig must start with this height's push
        let mut other = t.clone();
        other.height = 112;
        let e = check_answer(&other, &s, &ids, &CoinbaseFor { height: 112, ..good.clone() }).unwrap_err();
        assert!(e.contains("height push"), "{}", e);
        // a post-quantum answer must pay the miner's script, and a Sapling one must be shielded
        let pq = pq_script();
        let a = pq_answer(&t, &pq);
        let mut other_pq = pq.clone();
        other_pq[5] ^= 1;
        refused(&a, &PayoutKind::Pq(other_pq), &ids, "no output pays");
        refused(&CoinbaseFor { addresstype: "sapling".into(), ..a.clone() }, &s, &ids, "Sapling output");
        refused(&CoinbaseFor { addresstype: "pq".into(), ..good.clone() }, &PayoutKind::Pq(pq.clone()), &ids, "shielded");
    }

    /// `-ydf=5` on the pool node (D-2): the stock coinbase carries the optional fund share at
    /// vout[1]; the answer must carry the same output and pay the miner 95 % of the subsidy plus
    /// every fee, the same as the swap path.
    #[test]
    fn the_fund_output_must_be_the_stock_coinbases() {
        let t = template(include_str!("../tests/vectors/regtest-template-105.json"));
        let pq = pq_script();
        let mk = |cb: &Coinbase| {
            let data = cb.serialize();
            let mut h = dsha256(&data);
            h.reverse();
            (hex::encode(&data), hex::encode(h))
        };
        let mut cb = Coinbase::parse(&hex::decode(&t.coinbasetxn.data).unwrap()).unwrap();
        cb.vout[0].script_pubkey = pq.clone();
        let (data, hash) = mk(&cb);
        let good = CoinbaseFor {
            data,
            hash,
            height: 105,
            previousblockhash: t.previousblockhash.clone(),
            addresstype: "pq".into(),
            subsidy: 625_000_000,
            fund: 31_250_000,
            fundrule: "optional".into(),
            fees: 490,
            amount: 593_750_490,
            txcount: 2,
            flags: t.coinbaseaux.flags.clone(),
            sigops: 2,
        };
        let kind = PayoutKind::Pq(pq.clone());
        let c = check_answer(&t, &kind, &t.txids(), &good).unwrap();
        assert_eq!(c.flags, t.flags_bytes().unwrap());
        // the share moved to another script: refused
        let mut moved = cb.clone();
        moved.vout[1].script_pubkey[5] ^= 1;
        let (data, hash) = mk(&moved);
        let e = check_answer(&t, &kind, &t.txids(), &CoinbaseFor { data, hash, ..good.clone() }).unwrap_err();
        assert!(e.contains("fund output"), "{}", e);
        // no share where the stock coinbase has one: refused, whatever the amounts say
        let mut none = cb.clone();
        none.vout.truncate(1);
        none.vout[0].value += 31_250_000;
        let (data, hash) = mk(&none);
        let e = check_answer(&t, &kind, &t.txids(), &CoinbaseFor { data, hash, fund: 0, fundrule: "none".into(), amount: 625_000_490, ..good.clone() }).unwrap_err();
        assert!(e.contains("fund 0"), "{}", e);
    }

    /// Under load the oldest pair is renewed first, an address without a pair before all of them;
    /// the address string never decides (the reviewer's starvation case: `wanted.sort()` by
    /// address always served the same addresses first).
    #[test]
    fn order_serves_the_oldest_pair_first() {
        let t = template(include_str!("../tests/vectors/regtest-template-111-nofund.json"));
        let r = TemplateRef::of(&t);
        let old = TemplateRef { previousblockhash: "00".repeat(32), ..r.clone() };
        let quote = TemplateRef { flags: "24".into(), ..r.clone() };
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let entry = |subscribed: Instant, built: Option<(TemplateRef, Instant)>| Entry {
            kind: PayoutKind::Sapling,
            subscribers: 1,
            subscribed_at: subscribed,
            current: built.map(|(reference, b)| Arc::new(Pair { template: Arc::new(t.clone()), reference, coinbase: vec![], flags: vec![], amount: 0, built_at: b, read_at: b })),
            version: 0,
            in_flight: false,
            last_request: None,
            failed: None,
        };
        let mut m = HashMap::new();
        // a new block: every pair is void; "aaa" sorts first by name but its pair is the newest
        m.insert("aaa".to_string(), entry(at(0), Some((old.clone(), at(30)))));
        m.insert("mmm".to_string(), entry(at(0), Some((old.clone(), at(10)))));
        m.insert("zzz".to_string(), entry(at(0), Some((old.clone(), at(20)))));
        // two without a pair, the earlier subscriber first, both before any with a pair
        m.insert("yyy".to_string(), entry(at(5), None));
        m.insert("xxx".to_string(), entry(at(8), None));
        // a new quote ranks after every new-block need, however old its pair
        m.insert("bbb".to_string(), entry(at(0), Some((quote, at(1)))));
        let now = at(40);
        assert_eq!(order(&m, &r, now), ["yyy", "xxx", "mmm", "zzz", "aaa", "bbb"]);
        // once served, an address goes to the back of its class
        m.get_mut("mmm").unwrap().current = Some(Arc::new(Pair { template: Arc::new(t.clone()), reference: old, coinbase: vec![], flags: vec![], amount: 0, built_at: at(41), read_at: at(41) }));
        assert_eq!(order(&m, &r, now), ["yyy", "xxx", "zzz", "aaa", "mmm", "bbb"]);
    }

    /// `--max-paired-per-ip`: distinct paired addresses per source IP; more connections for an
    /// address the IP already holds, and other IPs, are not affected; the count falls with the
    /// last connection.
    #[test]
    fn paired_addresses_are_capped_per_ip() {
        let pairs = Pairs::new(2, 2, Duration::from_secs(60));
        let (ip, other): (IpAddr, IpAddr) = ("10.0.0.1".parse().unwrap(), "10.0.0.2".parse().unwrap());
        pairs.subscribe("z1", PayoutKind::Sapling, ip).unwrap();
        pairs.subscribe("z2", PayoutKind::Sapling, ip).unwrap();
        pairs.subscribe("z2", PayoutKind::Sapling, ip).unwrap();
        let e = pairs.subscribe("z3", PayoutKind::Sapling, ip).unwrap_err();
        assert_eq!(e, "Too many Sapling and post-quantum payout addresses from this IP (at most 2)");
        assert_eq!(pairs.paired_per_ip(ip), 2);
        assert!(!pairs.entries().contains_key("z3"), "the refused one left no entry");
        pairs.subscribe("z3", PayoutKind::Sapling, other).unwrap();
        // z2 has two connections: one leaving keeps the address held
        pairs.unsubscribe("z2", ip);
        assert!(pairs.subscribe("z3", PayoutKind::Sapling, ip).is_err());
        pairs.unsubscribe("z2", ip);
        assert_eq!(pairs.paired_per_ip(ip), 1);
        pairs.subscribe("z3", PayoutKind::Sapling, ip).unwrap();
        assert_eq!(pairs.entries()["z3"].subscribers, 2, "two IPs share the address's pair");
        pairs.unsubscribe("z1", ip);
        pairs.unsubscribe("z3", ip);
        pairs.unsubscribe("z3", other);
        assert_eq!(pairs.paired_per_ip(ip), 0);
        assert!(pairs.entries().is_empty());
    }

    /// "Method not found" switches paired jobs off; a probe is due only while it is off, and
    /// only once per period.
    #[test]
    fn availability_and_probes() {
        let pairs = Pairs::new(2, 8, Duration::from_millis(50));
        let rx = pairs.subscribe_versions();
        assert!(pairs.available() && !pairs.probe_due());
        pairs.set_available(false);
        assert!(!pairs.available());
        assert!(rx.has_changed().unwrap(), "paired miners are woken to leave");
        assert!(!pairs.probe_due(), "not at once after the answer");
        std::thread::sleep(Duration::from_millis(60));
        assert!(pairs.probe_due());
        assert!(!pairs.probe_due(), "one probe per period");
        pairs.set_available(true);
        assert!(pairs.available() && !pairs.probe_due());
    }

    #[test]
    fn need_orders_and_rate_limits() {
        let t = template(include_str!("../tests/vectors/regtest-template-111-nofund.json"));
        let r = TemplateRef::of(&t);
        let now = Instant::now();
        let mut e = Entry { kind: PayoutKind::Sapling, subscribers: 1, subscribed_at: now, current: None, version: 0, in_flight: false, last_request: None, failed: None };
        assert_eq!(need(&e, &r, now), Some(0), "no pair yet");
        e.in_flight = true;
        assert_eq!(need(&e, &r, now), None, "one request per address at a time");
        e.in_flight = false;
        let pair = |reference: TemplateRef| Some(Arc::new(Pair { template: Arc::new(t.clone()), reference, coinbase: vec![], flags: vec![], amount: 0, built_at: now, read_at: now }));
        e.current = pair(r.clone());
        assert_eq!(need(&e, &r, now), None, "the pair is current");
        e.current = pair(TemplateRef { previousblockhash: "00".repeat(32), ..r.clone() });
        assert_eq!(need(&e, &r, now), Some(0), "a new block: at once");
        e.current = pair(TemplateRef { flags: "24".into(), ..r.clone() });
        assert_eq!(need(&e, &r, now), Some(1), "a new quote: at once");
        e.current = pair(TemplateRef { txids: vec![], ..r.clone() });
        e.last_request = Some(now);
        assert_eq!(need(&e, &r, now), None, "a changed transaction set waits");
        assert_eq!(need(&e, &r, now + TX_REFRESH), Some(2), "and is renewed after TX_REFRESH");
        e.current = None;
        e.failed = Some((r.clone(), now));
        assert_eq!(need(&e, &r, now), None, "a failed template is not retried at once");
        assert_eq!(need(&e, &r, now + RETRY_AFTER), Some(0));
        let next = TemplateRef { previousblockhash: "11".repeat(32), ..r.clone() };
        assert_eq!(need(&e, &next, now), Some(0), "another template is asked for at once");
        e.subscribers = 0;
        assert_eq!(need(&e, &next, now), None);
    }
}

#[cfg(test)]
mod yugen_tests {
    use super::yugen_refusal;

    #[test]
    fn yugen_addresses_are_refused_with_a_reason() {
        for a in ["yqf1abc", "yqtf1abc", "yqrf1abc", "yqk1abc", "yqtk1", "yqrk1"] {
            assert!(yugen_refusal(a).unwrap().contains("Yugen"), "{}", a);
        }
        for a in ["ys1abc", "s1abc", "sqf1abc", "rqf1", "ybf1", "yrf1", "zregtestsapling1"] {
            assert!(yugen_refusal(a).is_none(), "{}", a);
        }
    }
}
