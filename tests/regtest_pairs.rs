// Copyright (c) 2026 The Ycash developers
// Distributed under the MIT software license, see the accompanying
// file LICENSE or https://www.opensource.org/licenses/mit-license.php .

//! The acceptance test of paired jobs (beta3 item 3, `docs/plans/beta3/getcoinbasetemplatefor.md`
//! §11 "3p"): `cargo test --features regtest --test regtest_pairs` with `YCASHD` set. One pool, the
//! Python (48,5) stratum miner, three kinds of username: a transparent miner on the swap path, a
//! Sapling and a post-quantum miner on paired jobs. Every block must pay its finder exactly the
//! subsidy, less the pool node's development-fund share, plus every fee of the block's
//! transactions; with `-ydf=5` the fund output must be the stock coinbase's.
//!
//! Nodes: P, the pool node (its wallet pays the fee-paying transactions; `mineraddress=` a wallet
//! t-addr; `-txindex`), and W, the miners' wallet (it owns the three payout addresses and shows
//! the Sapling note), peered with P. Phases, each block checked on chain:
//!
//! 1. `-ydf=0` (the default since beta3 item 4), one kind at a time, two fee-paying transactions
//!    in each template: transparent (35 characters), Sapling with a worker label (91 + 6),
//!    post-quantum FN-DSA (53), post-quantum SLH-DSA (53).
//! 2. The mixed pool: the three miners at once, one block each.
//!    Then phase 2p, the same pool with `--min-block-interval` 5 s (`paced` below): header times
//!    at least the interval apart, each block its finder's own and paid in full, a held block
//!    dropped when the node finds one first, one chain tip at the end.
//! 3. `-ydf=5` on P (and W): the three kinds again; each block pays 95 % of the subsidy plus all
//!    fees, and its fund output equals the stock coinbase's.
//!
//! Before phase 1, phase 0 binds yolo without a test source (the production path) and checks its
//! startup probe against the binary: a node with the call is reported and a Sapling username
//! accepted; one without it (6.22.0-beta2) refused with the clear message. After phase 3, with the
//! node's call only, phase 4 proves what the stub cannot: the answer's exact keys and every §8.3
//! check on it for each kind, a Sapling coinbase built per request, the error codes and strings
//! yolo matches on, `flags` from the node's current quote tag (an attestor set and `yed_setquote`;
//! a block for each of the four kinds, transparent, Sapling, FN-DSA and SLH-DSA, carries the tag,
//! and the same quote sent again makes no new job while a new price makes one), six Sapling addresses renewing under the cap of 2 with
//! `-rpcthreads=4` while the template poll, `getblockcount` and `submitblock` are timed, and
//! `coinbase-does-not-fit` for named transactions over the sigop limit.
//!
//! **Where the paired coinbases come from** (`YOLO_REGTEST_COINBASE`):
//! - `node`: the pool node's `getcoinbasetemplatefor` (a ycashd that has it, lane 3n);
//! - `stub`: the test-only stub of note §8.4 (W, started `-mineraddress=<the Sapling address>
//!   -minetolocalwallet=0` with P's `-ydf`, answers for the Sapling address; P's own coinbase with
//!   the miner's script at `vout[0]` answers for a post-quantum one), for nodes without the call;
//! - unset or `auto`: `node` when P serves the call, else `stub`, said in the log.
//!
//! Environment as `tests/regtest.rs` (`YCASHD`, `STRATUM_MINER`, `PYTHON`, `YOLO_REGTEST_SCRATCH`),
//! ports from `YOLO_REGTEST_PAIRS_RPC_BASE` / `YOLO_REGTEST_PAIRS_P2P_BASE` (default 26351 / 27351).
#![cfg(feature = "regtest")]

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use yolo::equihash::{Equihash, EquihashArg};
use yolo::pairs::classify_address;
use yolo::rpc::{CoinbaseFor, CoinbaseSource, RpcAuth, RpcClient, RpcError};
use yolo::template::BlockTemplate;
use yolo::tx::Coinbase;
use yolo::Config;

const VAULT_HEIGHT: u64 = 104;

fn log(msg: &str) {
    eprintln!("[regtest-pairs] {}", msg);
}

fn env_path(name: &str, default: PathBuf) -> PathBuf {
    std::env::var_os(name).map(PathBuf::from).unwrap_or(default)
}

fn zat(v: &Value) -> u64 {
    (v.as_f64().unwrap() * 1e8).round() as u64
}

struct Node {
    name: &'static str,
    datadir: PathBuf,
    p2p_port: u16,
    rpc: RpcClient,
    args: Vec<String>,
    child: Option<Child>,
}

impl Node {
    fn new(name: &'static str, scratch: &Path, rpc_port: u16, p2p_port: u16) -> Node {
        let datadir = scratch.join(name);
        let _ = std::fs::remove_dir_all(&datadir);
        std::fs::create_dir_all(datadir.join("regtest")).unwrap();
        let conf = format!("regtest=1\nserver=1\nrpcuser=u\nrpcpassword=p\nrpcport={}\nport={}\nlisten=1\ndiscover=0\nkeypool=1\nrpcallowip=127.0.0.1\nrpcthreads=8\n", rpc_port, p2p_port);
        std::fs::write(datadir.join("ycash.conf"), conf).unwrap();
        let rpc = RpcClient::new(&format!("http://127.0.0.1:{}", rpc_port), &RpcAuth { user: "u".into(), password: "p".into() });
        Node { name, datadir, p2p_port, rpc, args: Vec::new(), child: None }
    }

    fn start(&mut self, ycashd: &Path, base: &[String], extra: &[String]) {
        let stderr = std::fs::File::create(self.datadir.join("stderr.log")).unwrap();
        let child = Command::new(ycashd)
            .arg(format!("-datadir={}", self.datadir.display()))
            .args(base)
            .args(&self.args)
            .args(extra)
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .unwrap_or_else(|e| panic!("cannot start {}: {}", ycashd.display(), e));
        self.child = Some(child);
        let deadline = Instant::now() + Duration::from_secs(150);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(250));
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                panic!("node {} exited at start ({}); see {}/stderr.log", self.name, status, self.datadir.display());
            }
            if self.rpc.call("getblockcount", json!([])).is_ok() {
                return;
            }
        }
        panic!("node {} never answered RPC", self.name);
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = self.rpc.call("stop", json!([]));
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline && child.try_wait().unwrap().is_none() {
                std::thread::sleep(Duration::from_millis(200));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn call(&self, method: &str, params: Value) -> Value {
        self.rpc.call(method, params).unwrap_or_else(|e| panic!("{} {}: {}", self.name, method, e))
    }

    fn height(&self) -> u64 {
        self.call("getblockcount", json!([])).as_u64().unwrap()
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn wait_for(what: &str, timeout: Duration, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {}", what);
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// The test-only stub of note §8.4.
struct Stub {
    pool: RpcClient,
    helper: RpcClient,
    sapling: String,
    ydf: u64,
}

fn stale(m: &str) -> RpcError {
    RpcError::Node { code: -1, message: format!("template-stale: {}", m) }
}

impl Stub {
    /// A template of `rpc` on `prev` naming exactly `txids` (in any order), waiting a few seconds
    /// for the node's own 5 s template cache; `template-stale` otherwise.
    fn template_for(rpc: &RpcClient, prev: &str, txids: &[String]) -> Result<Value, RpcError> {
        let want: BTreeSet<&str> = txids.iter().map(String::as_str).collect();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let t = rpc.call("getblocktemplate", json!([]))?;
            let have: BTreeSet<&str> = t["transactions"].as_array().ok_or_else(|| fixture("no transactions"))?.iter().filter_map(|x| x["hash"].as_str()).collect();
            if t["previousblockhash"].as_str() == Some(prev) && have == want {
                return Ok(t);
            }
            if Instant::now() > deadline {
                return Err(if t["previousblockhash"].as_str() != Some(prev) { stale("previousblockhash is not the tip") } else { stale("transaction is not in the mempool") });
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

/// A malformed node answer, as an error of the stub (it returns `Result`, so it does not panic).
fn fixture(what: impl std::fmt::Display) -> RpcError {
    RpcError::Protocol(format!("stub: {}", what))
}

impl CoinbaseSource for Stub {
    fn coinbase_for(&self, address: &str, prev: &str, txids: &[String]) -> Result<CoinbaseFor, RpcError> {
        let coinbase_of = |t: &Value| -> Result<Vec<u8>, RpcError> { hex::decode(t["coinbasetxn"]["data"].as_str().ok_or_else(|| fixture("no coinbasetxn"))?).map_err(fixture) };
        let (t, data, addresstype) = if address == self.sapling {
            let t = Stub::template_for(&self.helper, prev, txids)?;
            let data = coinbase_of(&t)?;
            (t, data, "sapling")
        } else {
            let t = Stub::template_for(&self.pool, prev, txids)?;
            let v = self.pool.validateaddress(address)?;
            if v.pqscheme.is_none() {
                return Err(RpcError::Node { code: -5, message: "Address type cannot receive a coinbase: stub".into() });
            }
            let mut cb = Coinbase::parse(&coinbase_of(&t)?).map_err(fixture)?;
            let spk = hex::decode(v.script_pubkey.ok_or_else(|| fixture("no scriptPubKey"))?).map_err(fixture)?;
            cb.vout.first_mut().ok_or_else(|| fixture("a coinbase without outputs"))?.script_pubkey = spk;
            (t, cb.serialize(), "pq")
        };
        let cb = Coinbase::parse(&data).map_err(fixture)?;
        let mut fees = 0u64;
        for x in t["transactions"].as_array().ok_or_else(|| fixture("no transactions"))? {
            fees += x["fee"].as_u64().ok_or_else(|| fixture("a transaction without a fee"))?;
        }
        let claimed = cb.claimed_total().ok_or_else(|| fixture("the coinbase total overflows"))?;
        let fund = if addresstype == "sapling" { cb.transparent_total().ok_or_else(|| fixture("the outputs overflow"))? } else { cb.vout.iter().skip(1).map(|o| o.value).sum() };
        let height = t["height"].as_u64().and_then(|v| u32::try_from(v).ok()).ok_or_else(|| fixture("no height"))?;
        let fundrule = if fund == 0 {
            "none"
        } else if self.ydf > 0 {
            "optional"
        } else {
            "mandated"
        };
        let mut h = yolo::codec::dsha256(&data);
        h.reverse();
        Ok(CoinbaseFor {
            data: hex::encode(&data),
            hash: hex::encode(h),
            height,
            previousblockhash: prev.to_string(),
            addresstype: addresstype.into(),
            subsidy: claimed.saturating_sub(fees),
            fund,
            fundrule: fundrule.into(),
            fees,
            amount: claimed.saturating_sub(fund),
            txcount: txids.len(),
            flags: t["coinbaseaux"]["flags"].as_str().unwrap_or("").to_string(),
            sigops: 0,
        })
    }
}

/// Counts which addresses the pool asked a coinbase for.
struct Counting {
    inner: Arc<dyn CoinbaseSource>,
    asked: Mutex<HashMap<String, usize>>,
}

impl CoinbaseSource for Counting {
    fn coinbase_for(&self, address: &str, prev: &str, txids: &[String]) -> Result<CoinbaseFor, RpcError> {
        *self.asked.lock().unwrap_or_else(|e| e.into_inner()).entry(address.to_string()).or_insert(0) += 1;
        self.inner.coinbase_for(address, prev, txids)
    }
}

struct Miner {
    user: String,
    address: String,
    kind: &'static str,
    /// The payout script for a transparent or post-quantum address.
    script: Option<String>,
}

struct Ctx {
    p: Node,
    w: Node,
    python: PathBuf,
    miner: PathBuf,
}

impl Ctx {
    /// Sends `n` fee-paying transactions from P's wallet and waits until P's template and W's
    /// mempool carry them.
    fn send_fees(&self, n: usize) -> Vec<String> {
        let mut ids = Vec::new();
        for i in 0..n {
            let to = self.p.call("getnewaddress", json!([]));
            // the wallet can still pick an input of the block it has just connected; try again
            let mut tries = 0;
            let id = loop {
                match self.p.rpc.call("sendtoaddress", json!([to, 0.5 + i as f64])) {
                    Ok(v) => break v.as_str().unwrap().to_string(),
                    Err(e) if tries < 10 => {
                        log(&format!("sendtoaddress: {} (retrying)", e));
                        tries += 1;
                        std::thread::sleep(Duration::from_secs(1));
                    }
                    Err(e) => panic!("p sendtoaddress: {}", e),
                }
            };
            ids.push(id);
        }
        wait_for("the transactions in P's template and W's mempool", Duration::from_secs(60), || {
            let t = self.p.call("getblocktemplate", json!([]));
            let have: Vec<&str> = t["transactions"].as_array().unwrap().iter().filter_map(|x| x["hash"].as_str()).collect();
            let mempool = self.w.call("getrawmempool", json!([]));
            ids.iter().all(|id| have.contains(&id.as_str()) && mempool.as_array().unwrap().iter().any(|m| m == id))
        });
        ids
    }

    /// The block subsidy at `height`, from the node's `getblocksubsidy` (`miner` plus `founders`:
    /// both lines answer both keys), not from any coinbase the code under test handled.
    fn subsidy_at(&self, height: u64) -> u64 {
        let s = self.p.call("getblocksubsidy", json!([height]));
        zat(&s["miner"]) + zat(&s["founders"])
    }

    /// The fees of a block's transactions, from P's wallet (it sent all of them).
    fn block_fees(&self, block: &Value) -> u64 {
        block["tx"].as_array().unwrap()[1..].iter().map(|tx| zat(&json!(-self.p.call("gettransaction", json!([tx["txid"]]))["fee"].as_f64().unwrap()))).sum()
    }

    /// Checks the block at `height`: it pays `m` exactly `subsidy - fund + fees`, and with a fund
    /// share its fund output is `stock_fund` (value and script).
    fn check_block(&self, height: u64, m: &Miner, ydf: u64, stock_fund: Option<&(u64, String)>) -> u64 {
        let block = self.p.call("getblock", json!([height.to_string(), 2]));
        let cb = &block["tx"][0];
        let fees = self.block_fees(&block);
        let subsidy = self.subsidy_at(height);
        let fund = subsidy * ydf / 100;
        let want = subsidy - fund + fees;
        let vout = cb["vout"].as_array().unwrap();
        let paid = match m.kind {
            "sapling" => {
                let vb = cb["valueBalanceZat"].as_i64().unwrap_or_else(|| -(zat(&json!(-cb["valueBalance"].as_f64().unwrap())) as i64));
                assert_eq!(cb["vShieldedOutput"].as_array().map(Vec::len), Some(1), "{}: one Sapling output", m.user);
                let txid = cb["txid"].as_str().unwrap();
                wait_for("the Sapling note in W's wallet", Duration::from_secs(30), || {
                    self.w.call("z_listreceivedbyaddress", json!([m.address, 0])).as_array().unwrap().iter().any(|n| n["txid"] == txid)
                });
                let notes = self.w.call("z_listreceivedbyaddress", json!([m.address, 0]));
                let note = notes.as_array().unwrap().iter().find(|n| n["txid"] == txid).unwrap();
                let value = note["amountZat"].as_u64().unwrap_or_else(|| zat(&note["amount"]));
                assert_eq!(value as i64, -vb, "{}: the note is the coinbase's valueBalance", m.user);
                value
            }
            _ => {
                let script = m.script.as_deref().unwrap();
                let out: Vec<&Value> = vout.iter().filter(|o| o["scriptPubKey"]["hex"] == script).collect();
                assert_eq!(out.len(), 1, "{}: one output to the miner's script", m.user);
                out[0]["valueZat"].as_u64().unwrap_or_else(|| zat(&out[0]["value"]))
            }
        };
        assert_eq!(paid, want, "{}: block {} pays subsidy {} - fund {} + fees {}", m.user, height, subsidy, fund, fees);
        let funds: Vec<(u64, String)> = vout
            .iter()
            .filter(|o| m.script.as_deref().is_none_or(|s| o["scriptPubKey"]["hex"] != s))
            .map(|o| (o["valueZat"].as_u64().unwrap_or_else(|| zat(&o["value"])), o["scriptPubKey"]["hex"].as_str().unwrap().to_string()))
            .collect();
        match stock_fund {
            Some(f) => assert_eq!(funds, vec![f.clone()], "{}: the fund output is the stock coinbase's", m.user),
            None => assert!(funds.is_empty(), "{}: no fund output at -ydf=0: {:?}", m.user, funds),
        }
        log(&format!("block {} pays {} ({}) {} zat = {} - {} + {} fees; fund {:?}", height, m.user, m.kind, paid, subsidy, fund, fees, stock_fund));
        fees
    }

    fn mine(&self, pool: std::net::SocketAddr, user: &str) -> std::process::Child {
        Command::new(&self.python)
            .arg(&self.miner)
            .args(["--pool", &pool.to_string(), "--user", user, "--blocks", "1", "--equihash", "48,5"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }
}

fn finish(child: std::process::Child, user: &str) {
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && stdout.contains("accepted job"), "{}: the miner failed ({}):\n{}\n{}", user, out.status, stdout, String::from_utf8_lossy(&out.stderr));
}

async fn serve(ctx: &Ctx, source: Arc<Counting>) -> (std::net::SocketAddr, std::net::SocketAddr, tokio::task::JoinHandle<()>, yolo::state::State) {
    serve_with(ctx, Some(source as Arc<dyn CoinbaseSource>), yolo::Limits::default()).await
}

/// `source` None: the production path, the node's own `getcoinbasetemplatefor` after yolo's
/// startup probe.
async fn serve_with(ctx: &Ctx, source: Option<Arc<dyn CoinbaseSource>>, limits: yolo::Limits) -> (std::net::SocketAddr, std::net::SocketAddr, tokio::task::JoinHandle<()>, yolo::state::State) {
    let config = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        status_bind: Some("127.0.0.1:0".parse().unwrap()),
        payout: None,
        text: None,
        no_flags: false,
        password: None,
        equihash: EquihashArg::Fixed(Equihash::REGTEST),
        limits,
        test_coinbase_source: source,
    };
    let bound = yolo::bind(ctx.p.rpc.clone(), config).await.unwrap();
    let (addr, status, state) = (bound.addr, bound.status_addr.unwrap(), bound.state());
    let server = tokio::spawn(async move {
        let _ = bound.serve().await;
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while state.lock().template.is_none() {
        assert!(Instant::now() < deadline, "the poller never fetched a template");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    (addr, status, server, state)
}

/// The stock coinbase's fund output (value and script) at P's current template, if any.
fn stock_fund(p: &Node) -> Option<(u64, String)> {
    let t = p.call("getblocktemplate", json!([]));
    let cb = Coinbase::parse(&hex::decode(t["coinbasetxn"]["data"].as_str().unwrap()).unwrap()).unwrap();
    assert!(cb.vout.len() <= 2);
    cb.vout.get(1).map(|o| (o.value, hex::encode(&o.script_pubkey)))
}

/// One kind at a time, each block with two fee-paying transactions.
async fn sequential(ctx: &Ctx, miners: &[Miner], ydf: u64, source: Arc<Counting>) {
    let (pool, _, server, state) = serve(ctx, source.clone()).await;
    for m in miners {
        let fund = stock_fund(&ctx.p);
        assert_eq!(fund.is_some(), ydf > 0, "the stock coinbase carries a fund output exactly when -ydf > 0");
        let ids = ctx.send_fees(2);
        // the pool's own copy of the template (polled every second) carries them too
        let deadline = Instant::now() + Duration::from_secs(20);
        while !state.lock().template.as_ref().is_some_and(|t| ids.iter().all(|i| t.txids().contains(i))) {
            assert!(Instant::now() < deadline, "the pool's template never carried the transactions");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let before = ctx.p.height();
        let child = ctx.mine(pool, &m.user);
        let user = m.user.clone();
        tokio::task::spawn_blocking(move || finish(child, &user)).await.unwrap();
        wait_for("both nodes at the new height", Duration::from_secs(30), || ctx.w.height() > before && ctx.p.height() > before);
        let block = ctx.p.call("getblock", json!([(before + 1).to_string(), 2]));
        let mined: BTreeSet<String> = block["tx"].as_array().unwrap()[1..].iter().map(|t| t["txid"].as_str().unwrap().to_string()).collect();
        assert!(ids.iter().all(|i| mined.contains(i)), "{}: the block carries the fee-paying transactions", m.user);
        let fees = ctx.check_block(before + 1, m, ydf, fund.as_ref());
        assert!(fees > 0, "{}: the block pays fees", m.user);
        let asked = source.asked.lock().unwrap().get(&m.address).copied().unwrap_or(0);
        if m.kind == "p2pkh" {
            assert_eq!(asked, 0, "the transparent miner is on the swap path");
        } else {
            assert!(asked > 0, "{} is on the paired path", m.user);
        }
    }
    server.abort();
}

/// Phase 2p, block pacing (`--min-block-interval`, here 5 s; the Python miner has a block within a
/// second). The chain's clock is the real one here: the blocks of phases 1 and 2 were mined at the
/// wall clock, so the node's `curtime` is the clock and not median-time-past + 1.
///
/// a. Each kind alone, two blocks each with a fee-paying transaction: every block's header time is
///    at least the interval after its parent's, the block is the finder's own (paid in full, fees
///    included), and it reaches the chain although the miner left as soon as it was answered.
/// b. The three kinds at once, three tips: one block a tip, paying one of the three in full; the
///    other solutions are answered `true` and counted as shares.
/// c. A block mined by the node while one is held: the held block is dropped and never submitted.
///
/// At the end the node knows one chain tip only: no block was lost to a fork or orphaned.
async fn paced(ctx: &Ctx, miners: &[Miner], source: Arc<Counting>) {
    const N: u64 = 5;
    let limits = yolo::Limits { min_block_interval: Duration::from_secs(N), ..yolo::Limits::default() };
    let (pool, status, server, state) = serve_with(ctx, Some(source as Arc<dyn CoinbaseSource>), limits).await;
    let st = status_json(status).await;
    assert_eq!((st["minBlockInterval"].clone(), st["held"].clone()), (json!(N), Value::Null), "{}", st);
    let time_of = |h: u64| ctx.p.call("getblockheader", json!([ctx.p.call("getblockhash", json!([h]))]))["time"].as_u64().unwrap();
    let payee = |h: u64| -> &Miner {
        let cb = &ctx.p.call("getblock", json!([h.to_string(), 2]))["tx"][0];
        if cb["vShieldedOutput"].as_array().is_some_and(|v| !v.is_empty()) {
            miners.iter().find(|m| m.kind == "sapling").unwrap()
        } else {
            miners.iter().find(|m| m.script.as_deref().is_some_and(|s| cb["vout"].as_array().unwrap().iter().any(|o| o["scriptPubKey"]["hex"] == s))).expect("a block paying one of the miners")
        }
    };
    let first = ctx.p.height() + 1;
    let started = Instant::now();
    let mut blocks = 0u64;
    let mut candidates = 0u64;

    // a. one kind at a time
    for m in miners {
        for _ in 0..2 {
            let ids = ctx.send_fees(1);
            let deadline = Instant::now() + Duration::from_secs(20);
            while !state.lock().template.as_ref().is_some_and(|t| ids.iter().all(|i| t.txids().contains(i))) {
                assert!(Instant::now() < deadline, "the pool's template never carried the transaction");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let before = ctx.p.height();
            let child = ctx.mine(pool, &m.user);
            let user = m.user.clone();
            // the miner is answered `true` and leaves; its block is still held
            tokio::task::spawn_blocking(move || finish(child, &user)).await.unwrap();
            // (that it was held is shown below by the header times and the elapsed time, not by
            // looking at the node at a moment the test would have to guess)
            wait_for("the held block on chain", Duration::from_secs(N + 20), || ctx.p.height() > before);
            wait_for("W at the new height", Duration::from_secs(30), || ctx.w.height() > before);
            assert_eq!(payee(before + 1).user, m.user, "the block is its finder's own");
            assert!(ctx.check_block(before + 1, m, 0, None) > 0, "{}: the block pays its fee", m.user);
            blocks += 1;
            candidates += 1;
        }
    }

    // b. the three kinds at once
    let mut winners = BTreeSet::new();
    for _ in 0..3 {
        let before = ctx.p.height();
        let children: Vec<_> = miners.iter().map(|m| (ctx.mine(pool, &m.user), m.user.clone())).collect();
        for (child, user) in children {
            // every miner's solution is answered `true`, whether or not it is the held block
            tokio::task::spawn_blocking(move || finish(child, &user)).await.unwrap();
        }
        wait_for("the held block on chain", Duration::from_secs(N + 20), || ctx.p.height() > before);
        wait_for("W at the new height", Duration::from_secs(30), || ctx.w.height() > before);
        // no second block for the tip follows
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(ctx.p.height(), before + 1, "one block a tip");
        let m = payee(before + 1);
        ctx.check_block(before + 1, m, 0, None);
        winners.insert(m.kind);
        blocks += 1;
        candidates += miners.len() as u64;
    }
    log(&format!("paced, mixed: blocks went to {:?}", winners));

    let last = ctx.p.height();
    assert_eq!(last, first + blocks - 1);
    let times: Vec<u64> = (first - 1..=last).map(time_of).collect();
    let gaps: Vec<u64> = times.windows(2).map(|w| w[1] - w[0]).collect();
    log(&format!("paced: {} blocks in {:.0} s, header-time gaps {:?}", blocks, started.elapsed().as_secs_f64(), gaps));
    assert!(gaps.iter().all(|g| *g >= N), "every header time is at least {} s after its parent's: {:?}", N, gaps);
    assert!(started.elapsed() >= Duration::from_secs(N * blocks), "and the blocks arrived no faster");
    let st = status_json(status).await;
    assert_eq!(
        (st["accepted"].clone(), st["rejected"].clone(), st["stale"].clone(), st["heldStale"].clone(), st["shares"].clone(), st["paceCandidates"].clone()),
        (json!(blocks), json!(0), json!(0), json!(0), json!(candidates), json!(candidates)),
        "every block held was accepted, every solution credited: {}",
        st
    );

    // c. the node finds a block while one is held
    let m = &miners[0];
    let before = ctx.p.height();
    let child = ctx.mine(pool, &m.user);
    let user = m.user.clone();
    tokio::task::spawn_blocking(move || finish(child, &user)).await.unwrap();
    assert_eq!(status_json(status).await["held"]["height"], json!(before + 1));
    ctx.p.call("generate", json!([1]));
    tokio::time::sleep(Duration::from_secs(N + 3)).await;
    assert_eq!(ctx.p.height(), before + 1, "only the node's block: the held one was never submitted");
    let cb = &ctx.p.call("getblock", json!([(before + 1).to_string(), 2]))["tx"][0];
    assert!(!cb["vout"].as_array().unwrap().iter().any(|o| Some(o["scriptPubKey"]["hex"].as_str().unwrap_or("")) == m.script.as_deref()), "the block at {} is the node's", before + 1);
    let st = status_json(status).await;
    assert_eq!((st["heldStale"].clone(), st["held"].clone(), st["accepted"].clone(), st["rejected"].clone()), (json!(1), Value::Null, json!(blocks), json!(0)), "{}", st);
    let tips = ctx.p.call("getchaintips", json!([]));
    assert_eq!(tips.as_array().map(Vec::len), Some(1), "one chain tip, nothing orphaned: {}", tips);
    wait_for("W at the tip", Duration::from_secs(30), || ctx.w.height() == before + 1);
    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn paired_jobs_pay_each_miner_in_full() {
    let Some(ycashd) = std::env::var_os("YCASHD").map(PathBuf::from) else {
        eprintln!("SKIP: YCASHD is not set");
        return;
    };
    let ws = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
    let python = env_path("PYTHON", ws.join(".venv/bin/python"));
    let miner = env_path("STRATUM_MINER", ws.join("ycash-dd/contrib/yellowback/devnet/stratum-miner"));
    let scratch = env_path("YOLO_REGTEST_SCRATCH", std::env::temp_dir().join("yolo-regtest")).join("pairs");
    let rpc_base: u16 = std::env::var("YOLO_REGTEST_PAIRS_RPC_BASE").ok().and_then(|v| v.parse().ok()).unwrap_or(26351);
    let p2p_base: u16 = std::env::var("YOLO_REGTEST_PAIRS_P2P_BASE").ok().and_then(|v| v.parse().ok()).unwrap_or(27351);
    std::fs::create_dir_all(&scratch).unwrap();
    let version = String::from_utf8_lossy(&Command::new(&ycashd).arg("-version").output().unwrap().stdout).to_string();
    let six = version.contains(" v6.");
    log(&format!("{} ({})", version.lines().next().unwrap_or(""), if six { "6.x line" } else { "v4.5.0 line" }));
    let mut base: Vec<String> = ["5ba81b19", "76b809bb", "374d694f", "8e471bd6", "66314da3", "19bd2d2f"].iter().map(|b| format!("-nuparams={}:1", b)).collect();
    base.push(format!("-nuparams=6d5b7a31:{}", VAULT_HEIGHT));
    base.push("-pqfalcon=1".into());
    if six {
        for d in ["getnewaddress", "z_getnewaddress"] {
            base.push(format!("-allowdeprecated={}", d));
        }
    }
    let pool_args: Vec<String> = std::env::var("YOLO_REGTEST_POOL_NODE_ARGS").unwrap_or_default().split_whitespace().map(String::from).collect();

    let mut p = Node::new("p", &scratch, rpc_base, p2p_base);
    let mut w = Node::new("w", &scratch, rpc_base + 1, p2p_base + 1);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    p.start(&ycashd, &base, &[format!("-mocktime={}", now - 3600), "-txindex".into()]);
    let mineraddress = p.call("getnewaddress", json!([])).as_str().unwrap().to_string();
    p.call("generate", json!([VAULT_HEIGHT + 1]));
    p.stop();
    // W mints the miners' addresses once it has the chain (a post-quantum address needs the vault
    // upgrade active at the next block)
    p.start(&ycashd, &base, &[format!("-mineraddress={}", mineraddress), "-txindex".into()]);
    w.start(&ycashd, &base, &[format!("-connect=127.0.0.1:{}", p.p2p_port)]);
    let h = p.height();
    wait_for("W synced to P", Duration::from_secs(60), || w.height() == h);
    let t_addr = w.call("getnewaddress", json!([])).as_str().unwrap().to_string();
    let z_addr = w.call("z_getnewaddress", json!(["sapling"])).as_str().unwrap().to_string();
    let pqf = w.call("getnewaddress", json!(["pq"])).as_str().unwrap().to_string();
    let pqs = w.call("getnewaddress", json!(["pqslh"])).as_str().unwrap().to_string();
    w.stop();
    p.stop();
    log(&format!("miners: {} ({}), {} ({}), {} ({}), {} ({})", t_addr, t_addr.len(), z_addr, z_addr.len(), pqf, pqf.len(), pqs, pqs.len()));

    let script_rpc = w.rpc.clone();
    let script = move |a: &str| script_rpc.validateaddress(a).ok().and_then(|v| v.script_pubkey);
    let start = |p: &mut Node, w: &mut Node, ydf: u64| {
        p.args = vec![format!("-mineraddress={}", mineraddress), format!("-ydf={}", ydf), "-txindex".into()];
        p.args.extend(pool_args.iter().cloned());
        p.start(&ycashd, &base, &[]);
        // W: the miners' wallet, and the stub's helper node (its coinbase pays the Sapling address)
        w.args = vec![format!("-mineraddress={}", z_addr), "-minetolocalwallet=0".into(), format!("-ydf={}", ydf), format!("-connect=127.0.0.1:{}", p.p2p_port)];
        w.start(&ycashd, &base, &[]);
        let h = p.height();
        wait_for("W synced to P", Duration::from_secs(60), || w.height() == h);
    };
    start(&mut p, &mut w, 0);

    let mode = std::env::var("YOLO_REGTEST_COINBASE").unwrap_or_else(|_| "auto".into());
    let has_call = p.rpc.has_coinbase_call().unwrap();
    let use_node = match mode.as_str() {
        "node" => {
            assert!(has_call, "YOLO_REGTEST_COINBASE=node, but {} does not serve getcoinbasetemplatefor", ycashd.display());
            true
        }
        "stub" => false,
        _ => has_call,
    };
    log(&format!("paired coinbases from {}", if use_node { "the node's getcoinbasetemplatefor" } else { "the TEST STUB (note §8.4): this run does not prove the node call" }));
    let make_source = |p: &Node, w: &Node, ydf: u64| -> Arc<Counting> {
        let inner: Arc<dyn CoinbaseSource> = if use_node { Arc::new(p.rpc.clone()) } else { Arc::new(Stub { pool: p.rpc.clone(), helper: w.rpc.clone(), sapling: z_addr.clone(), ydf }) };
        Arc::new(Counting { inner, asked: Mutex::new(HashMap::new()) })
    };

    let mut ctx = Ctx { p, w, python, miner };
    let next = ctx.p.height() + 1;
    log(&format!("subsidy {} zat at height {} (getblocksubsidy)", ctx.subsidy_at(next), next));

    // 0. yolo's own startup probe against this binary (no test source: the production path)
    match ctx.p.rpc.call("getcoinbasetemplatefor", json!([])) {
        Err(RpcError::Node { code, message }) => log(&format!("probe: getcoinbasetemplatefor without arguments answers code {} \"{}\"", code, message.lines().next().unwrap_or(""))),
        other => panic!("probe: unexpected {:?}", other),
    }
    {
        let (pool, status, server, _) = serve_with(&ctx, None, yolo::Limits::default()).await;
        let st = status_json(status).await;
        assert_eq!(st["coinbaseCall"], json!(has_call), "/status coinbaseCall follows the probe: {}", st["coinbaseCall"]);
        let answer = stratum_authorize(pool, &z_addr).await;
        if has_call {
            assert_eq!(answer["result"], json!(true), "a Sapling username is accepted: {}", answer);
        } else {
            assert_eq!(answer["error"], json!("Invalid address: this pool cannot pay sapling addresses now (its node does not serve getcoinbasetemplatefor)"), "{}", answer);
        }
        assert_eq!(stratum_authorize(pool, &t_addr).await["result"], json!(true), "a transparent username is accepted either way");
        server.abort();
        log(&format!("phase 0 (startup probe): the node {} the call; /status coinbaseCall {}; Sapling authorize {}", if has_call { "serves" } else { "does not serve" }, st["coinbaseCall"], answer));
    }

    let miners = vec![
        Miner { user: t_addr.clone(), address: t_addr.clone(), kind: "p2pkh", script: script(&t_addr) },
        Miner { user: format!("{}.rig-1", z_addr), address: z_addr.clone(), kind: "sapling", script: None },
        Miner { user: pqf.clone(), address: pqf.clone(), kind: "pq", script: script(&pqf) },
        Miner { user: pqs.clone(), address: pqs.clone(), kind: "pq", script: script(&pqs) },
    ];
    assert!(miners.iter().all(|m| m.kind == "sapling" || m.script.is_some()));

    // 1. -ydf=0, one kind at a time
    sequential(&ctx, &miners, 0, make_source(&ctx.p, &ctx.w, 0)).await;
    log("phase 1 (-ydf=0, one kind at a time): ok");

    // 2. the mixed pool: three miners at once, one block each
    let source = make_source(&ctx.p, &ctx.w, 0);
    let (pool, status, server, _) = serve(&ctx, source.clone()).await;
    ctx.send_fees(2);
    let before = ctx.p.height();
    let mixed = [&miners[0], &miners[1], &miners[2]];
    let children: Vec<_> = mixed.iter().map(|m| (ctx.mine(pool, &m.user), m.user.clone())).collect();
    // each kind shows on /status while its miner is connected
    let mut seen = BTreeSet::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while seen.len() < 3 && Instant::now() < deadline {
        let st = status_json(status).await;
        for k in ["p2pkh", "sapling", "pq"] {
            if st["minersByKind"][k].as_u64().unwrap_or(0) == 1 {
                seen.insert(k);
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // (a miner may have found its block before the first look; tests/paired.rs checks /status)
    log(&format!("mixed pool /status listed {:?}", seen));
    assert!(seen.contains("sapling") && seen.contains("pq"), "/status lists the paired miners: {:?}", seen);
    // fee-paying transactions keep arriving while they mine
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let feeder = {
        let (rpc, stop) = (ctx.p.rpc.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut last = 0;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                let h = rpc.call("getblockcount", json!([])).ok().and_then(|v| v.as_u64()).unwrap_or(0);
                if h != last {
                    last = h;
                    if let Ok(to) = rpc.call("getnewaddress", json!([])) {
                        let _ = rpc.call("sendtoaddress", json!([to, 0.25]));
                    }
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        })
    };
    for (child, user) in children {
        tokio::task::spawn_blocking(move || finish(child, &user)).await.unwrap();
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    feeder.join().unwrap();
    server.abort();
    let after = ctx.p.height();
    assert_eq!(after, before + 3, "three blocks, one per miner");
    wait_for("W at the tip", Duration::from_secs(30), || ctx.w.height() == after);
    let mut paid = BTreeSet::new();
    for h in before + 1..=after {
        let cb = &ctx.p.call("getblock", json!([h.to_string(), 2]))["tx"][0];
        let m = if cb["vShieldedOutput"].as_array().is_some_and(|v| !v.is_empty()) {
            &miners[1]
        } else {
            *mixed.iter().find(|m| m.script.as_deref().is_some_and(|s| cb["vout"].as_array().unwrap().iter().any(|o| o["scriptPubKey"]["hex"] == s))).expect("a block paying one of the three miners")
        };
        ctx.check_block(h, m, 0, None);
        paid.insert(m.kind);
    }
    assert_eq!(paid.len(), 3, "each of the three kinds found a block");
    log("phase 2 (the mixed pool): ok");

    // 2p. the same pool with --min-block-interval (phases 1 and 2 above ran without it)
    paced(&ctx, &miners[..3], make_source(&ctx.p, &ctx.w, 0)).await;
    log("phase 2p (block pacing): ok");

    // 3. -ydf=5 on the pool node (D-2): both paths pay 95 % of the subsidy plus all fees
    ctx.p.stop();
    ctx.w.stop();
    start(&mut ctx.p, &mut ctx.w, 5);
    sequential(&ctx, &miners[..3], 5, make_source(&ctx.p, &ctx.w, 5)).await;
    log("phase 3 (-ydf=5): ok");

    if use_node {
        real_call_facts(&mut ctx, &ycashd, &base, &miners, &z_addr).await;
    }
    ctx.p.stop();
    ctx.w.stop();
}

/// Phase 4, against a node that serves the call (the facts the stub could not prove): the answer's
/// exact shape and the §8.3 checks on it for every kind, a Sapling coinbase built per request, the
/// error codes and strings yolo matches on, `flags` from the node's current quote tag, the
/// request cap against the node's `-rpcthreads`, and `coinbase-does-not-fit`. P runs at `-ydf=5`.
async fn real_call_facts(ctx: &mut Ctx, ycashd: &Path, base: &[String], miners: &[Miner], z_addr: &str) {
    const KEYS: [&str; 13] = ["data", "hash", "height", "previousblockhash", "addresstype", "subsidy", "fund", "fundrule", "fees", "amount", "txcount", "flags", "sigops"];
    let call = |p: &Node, address: &str, prev: &str, txids: &[String]| p.rpc.call("getcoinbasetemplatefor", json!([address, {"previousblockhash": prev, "txids": txids}]));
    ctx.send_fees(2);
    let tv = ctx.p.call("getblocktemplate", json!([]));
    let t: BlockTemplate = serde_json::from_value(tv.clone()).unwrap();
    let (prev, ids) = (t.previousblockhash.clone(), t.txids());
    assert!(!ids.is_empty());

    // (1) the shape, and every check of §8.3, for each kind
    for m in miners {
        let raw = call(&ctx.p, &m.address, &prev, &ids).unwrap_or_else(|e| panic!("{}: {}", m.user, e));
        let keys: BTreeSet<&str> = raw.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, KEYS.iter().copied().collect::<BTreeSet<_>>(), "{}: the answer's keys", m.user);
        let a: CoinbaseFor = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!((a.addresstype.as_str(), a.fundrule.as_str()), (m.kind, "optional"), "{}", m.user);
        assert_eq!(a.amount, a.subsidy - a.fund + a.fees);
        let kind = classify_address(&ctx.p.rpc, &m.address).unwrap().unwrap();
        yolo::pairs::check_answer(&t, &kind, &ids, &a).unwrap_or_else(|e| panic!("{}: yolo refuses the node's answer: {}", m.user, e));
        let mut shown = raw.clone();
        shown["data"] = json!(format!("<{} bytes>", a.data.len() / 2));
        log(&format!("answer for {} ({}): {}", m.user, m.kind, shown));
    }
    // (3) a Sapling coinbase built per request: two requests for one address, and one for another
    // address, give three different valid coinbases
    let z2 = ctx.w.call("z_getnewaddress", json!(["sapling"])).as_str().unwrap().to_string();
    let mut seen = BTreeSet::new();
    for z in [z_addr, z_addr, z2.as_str()] {
        let a: CoinbaseFor = serde_json::from_value(call(&ctx.p, z, &prev, &ids).unwrap()).unwrap();
        yolo::pairs::check_answer(&t, &yolo::pairs::PayoutKind::Sapling, &ids, &a).unwrap();
        let cb = Coinbase::parse(&hex::decode(&a.data).unwrap()).unwrap();
        assert_eq!(cb.sapling_outputs, 1);
        assert!(seen.insert(a.data.clone()), "each request builds a new Sapling output");
    }
    log("per-request Sapling coinbases: three requests, three different valid coinbases (one output each)");

    // (1) the errors yolo matches on, with their codes
    let err = |r: Result<Value, RpcError>| match r {
        Err(RpcError::Node { code, message }) => (code, message),
        other => panic!("expected a node error, got {:?}", other),
    };
    let (code, msg) = err(call(&ctx.p, z_addr, &"11".repeat(32), &ids));
    assert_eq!((code, msg.as_str()), (-1, "template-stale: previousblockhash is not the tip"));
    assert!(RpcError::Node { code, message: msg.clone() }.is_template_stale());
    let ghost = "ab".repeat(32);
    let (code, msg) = err(call(&ctx.p, z_addr, &prev, std::slice::from_ref(&ghost)));
    assert_eq!((code, msg.clone()), (-1, format!("template-stale: transaction is not in the mempool: {}", ghost)));
    assert!(RpcError::Node { code, message: msg }.is_template_stale());
    let (code, msg) = err(call(&ctx.p, z_addr, &prev, &[ids[0].clone(), ids[0].clone()]));
    assert_eq!((code, msg.clone()), (-8, format!("txids names a transaction twice: {}", ids[0])));
    // the node checks the mempool before duplicates: an absent id named twice is template-stale
    let (code, msg) = err(call(&ctx.p, z_addr, &prev, &[ghost.clone(), ghost.clone()]));
    assert_eq!((code, msg.clone()), (-1, format!("template-stale: transaction is not in the mempool: {}", ghost)));
    let (code, msg) = err(ctx.p.rpc.call("getcoinbasetemplatefor", json!([z_addr, {"previousblockhash": prev, "txids": ids, "amount": 1}])));
    assert_eq!((code, msg.as_str()), (-8, "template must be an object with exactly the keys previousblockhash and txids"));
    log("errors: template-stale (tip, mempool; code -1), duplicate (-8, after the mempool check), an extra key (-8): as yolo matches them");

    // (4) and (6): the YED attestor set and a quote; P restarted with -rpcthreads=4 (the cap 2 plus 2)
    let created = ctx.p.call("set_create", json!([{"seats": 15, "unlockthreshold": 1, "cancelthreshold": 1, "slashthreshold": 1, "open": true, "maturity": 2}]));
    let setid = created["setid"].as_str().unwrap().to_string();
    wait_for("set_create in the mempool", Duration::from_secs(30), || !ctx.p.call("getrawmempool", json!([])).as_array().unwrap().is_empty());
    ctx.p.call("generate", json!([1]));
    let h = ctx.p.height();
    wait_for("W at the set's height", Duration::from_secs(30), || ctx.w.height() == h);
    ctx.p.stop();
    ctx.w.stop();
    let quote_args = ["-experimentalfeatures".to_string(), "-yellowbacksigmaref=0".to_string(), format!("-yellowbackattestorset={}", setid)];
    ctx.p.args.extend(quote_args.iter().cloned());
    ctx.p.args.push("-rpcthreads=4".into());
    ctx.w.args.extend(quote_args.iter().cloned());
    ctx.p.start(ycashd, base, &[]);
    ctx.w.start(ycashd, base, &[]);
    wait_for("W synced to P", Duration::from_secs(60), || ctx.w.height() == ctx.p.height());
    ctx.p.call("yed_setquote", json!([50_000, 1]));
    let mut flags = String::new();
    wait_for("a 37-byte quote tag in the template", Duration::from_secs(30), || {
        flags = ctx.p.call("getblocktemplate", json!([]))["coinbaseaux"]["flags"].as_str().unwrap_or("").to_string();
        flags.len() == 74
    });
    let tv = ctx.p.call("getblocktemplate", json!([]));
    let t: BlockTemplate = serde_json::from_value(tv).unwrap();
    let a: CoinbaseFor = serde_json::from_value(call(&ctx.p, z_addr, &t.previousblockhash, &t.txids()).unwrap()).unwrap();
    let next_flags = ctx.p.call("getblocktemplate", json!([]))["coinbaseaux"]["flags"].as_str().unwrap().to_string();
    assert_eq!(a.flags, next_flags, "flags is the node's current quote tag");
    let cb = Coinbase::parse(&hex::decode(&a.data).unwrap()).unwrap();
    let push = yolo::tag::height_push(t.height);
    assert!(cb.script_sig.starts_with(&push) && cb.script_sig[push.len()..].windows(37).any(|w| w == hex::decode(&a.flags).unwrap().as_slice()), "the scriptSig carries the tag after the height push");
    // a block for every kind of miner with the quote (beta4 item 3a): transparent on the swap path,
    // Sapling, FN-DSA and SLH-DSA on paired jobs; the chain reads each block's tag back
    let first = ctx.p.height() + 1;
    sequential(ctx, miners, 5, Arc::new(Counting { inner: Arc::new(ctx.p.rpc.clone()), asked: Mutex::new(HashMap::new()) })).await;
    for (h, m) in (first..).zip(miners) {
        let tag = ctx.p.call("yed_gettag", json!([h.to_string()]));
        assert_eq!((tag["found"].clone(), tag["kind"].clone(), tag["priceMicroUsd"].clone()), (json!(true), json!("quote"), json!(50_000)), "{}: the block's tag: {}", m.user, tag);
        log(&format!("quote: block {} for {} ({}) carries the tag: {}", h, m.user, m.kind, tag));
    }
    log(&format!("quote: flags {} from the node's current tag; blocks {}..{} (every kind) carry it", a.flags, first, ctx.p.height()));

    // (5) what a quote does to the pool's jobs: the poller makes new work (one `mining.notify`
    // with clean_jobs to every miner) only when `coinbaseaux.flags` changes. The same price again
    // (a heartbeat) rebuilds the node's template (quoteGeneration) but leaves the tag's bytes, so
    // no new job; a new price is one new job.
    {
        let (_, _, server, state) = serve_with(ctx, None, yolo::Limits::default()).await;
        let settle = || tokio::time::sleep(Duration::from_secs(3));
        settle().await;
        let g0 = state.lock().generation;
        ctx.p.call("yed_setquote", json!([50_000, 1]));
        settle().await;
        let g1 = state.lock().generation;
        assert_eq!(g1, g0, "the same quote again makes no new work");
        ctx.p.call("yed_setquote", json!([50_050, 1]));
        settle().await;
        let g2 = state.lock().generation;
        assert_eq!(g2, g1 + 1, "a new price makes exactly one new work (flags changed)");
        ctx.p.call("yed_setquote", json!([50_000, 1]));
        settle().await;
        server.abort();
        log(&format!("quote and jobs: same price again -> {} new work, a new price -> {} new work (clean_jobs to every miner)", g1 - g0, g2 - g1));
    }

    // (6) the request cap against -rpcthreads=4: six Sapling addresses renewing at once while the
    // template poll, getblockcount and submitblock are timed on their own connections
    let timed = Arc::new(Timed { inner: Arc::new(ctx.p.rpc.clone()), in_flight: Mutex::new((0, 0)), durations: Mutex::new(Vec::new()) });
    let limits = yolo::Limits { coinbase_concurrency: 2, ..yolo::Limits::default() };
    let (_, _, server, state) = serve_with(ctx, Some(timed.clone() as Arc<dyn CoinbaseSource>), limits).await;
    let ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();
    let zs: Vec<String> = (0..6).map(|_| ctx.w.call("z_getnewaddress", json!(["sapling"])).as_str().unwrap().to_string()).collect();
    for z in &zs {
        state.pairs.subscribe(z, yolo::pairs::PayoutKind::Sapling, ip).unwrap();
    }
    let all_paired = |state: &yolo::state::State, tip: &str| zs.iter().all(|z| state.pairs.current(z).is_some_and(|(_, p)| p.reference.previousblockhash == tip));
    let tip = ctx.p.call("getbestblockhash", json!([])).as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !all_paired(&state, &tip) {
        assert!(Instant::now() < deadline, "six pairs on the first tip");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    timed.durations.lock().unwrap().clear();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let prober = {
        let (rpc, stop) = (ctx.p.rpc.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut worst = [Duration::ZERO; 3];
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                for (i, (m, params)) in [("getblocktemplate", json!([])), ("getblockcount", json!([])), ("submitblock", json!(["00"]))].into_iter().enumerate() {
                    let at = Instant::now();
                    let _ = rpc.call(m, params);
                    worst[i] = worst[i].max(at.elapsed());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            worst
        })
    };
    ctx.p.call("generate", json!([1]));
    let tip = ctx.p.call("getbestblockhash", json!([])).as_str().unwrap().to_string();
    let started = Instant::now();
    let deadline = started + Duration::from_secs(120);
    while !all_paired(&state, &tip) {
        assert!(Instant::now() < deadline, "six pairs on the new tip");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let renewed = started.elapsed();
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let worst = prober.join().unwrap();
    server.abort();
    let peak = timed.in_flight.lock().unwrap().1;
    let durations = timed.durations.lock().unwrap().clone();
    assert!(peak <= 2, "at most --coinbase-concurrency requests in flight: {}", peak);
    log(&format!(
        "rpcthreads=4, cap 2: six Sapling pairs renewed in {:?}; peak in flight {}; request times {:?}; worst getblocktemplate {:?}, getblockcount {:?}, submitblock {:?}",
        renewed, peak, durations, worst[0], worst[1], worst[2]
    ));
    assert!(worst.iter().all(|d| *d < Duration::from_secs(2)), "no mining call waited behind the proofs: {:?}", worst);

    // (5) coinbase-does-not-fit: six transactions of 3,980 sigops each (each under the per-transaction
    // policy limit of 4,000) named together exceed the block's 20,000
    let to = ctx.p.call("getnewaddress", json!([]));
    let spk = ctx.p.call("validateaddress", json!([to]))["scriptPubKey"].as_str().unwrap().to_string();
    // distinct, confirmed, non-coinbase coins of P's wallet (the fee-paying sends' outputs): a
    // wallet-funded transaction may pick a coin an earlier one of these spends
    let coins: Vec<Value> = ctx.p.call("listunspent", json!([1])).as_array().unwrap().iter().filter(|u| u["generated"] != json!(true) && zat(&u["amount"]) >= 1_000_000).take(6).cloned().collect();
    assert_eq!(coins.len(), 6, "six spendable non-coinbase coins");
    let mut heavy = Vec::new();
    for u in &coins {
        let value = (zat(&u["amount"]) - 10_000) as f64 / 1e8;
        let raw = ctx.p.call("createrawtransaction", json!([[{"txid": u["txid"], "vout": u["vout"]}], {to.as_str().unwrap(): value}]));
        let raw = raw.as_str().unwrap().replacen(&format!("19{}", spk), &format!("c7{}", "ae".repeat(199)), 1);
        let signed = ctx.p.call("signrawtransaction", json!([raw]));
        assert_eq!(signed["complete"], json!(true));
        heavy.push(ctx.p.call("sendrawtransaction", json!([signed["hex"]])).as_str().unwrap().to_string());
    }
    let prev = ctx.p.call("getbestblockhash", json!([])).as_str().unwrap().to_string();
    let (code, msg) = err(call(&ctx.p, z_addr, &prev, &heavy));
    assert_eq!(code, -1);
    assert!(msg.starts_with("coinbase-does-not-fit: sigops ") && msg.ends_with(" exceed 20000"), "{}", msg);
    let t = ctx.p.call("getblocktemplate", json!([]));
    let in_template = heavy.iter().filter(|id| t["transactions"].as_array().unwrap().iter().any(|x| x["hash"] == **id)).count();
    let template_sigops: u64 = t["transactions"].as_array().unwrap().iter().map(|x| x["sigops"].as_u64().unwrap()).sum();
    log(&format!(
        "coinbase-does-not-fit: \"{}\" (code -1) for the six named together; the node's template at that moment carries {} of them ({} sigops of {})",
        msg, in_template, template_sigops, t["sigoplimit"]
    ));
    let a: CoinbaseFor = serde_json::from_value(call(&ctx.p, z_addr, &prev, &serde_json::from_value::<BlockTemplate>(t).unwrap().txids()).unwrap()).unwrap();
    log(&format!("the same template's own transaction set still gets a coinbase ({} sigops)", a.sigops));
}

/// Times each request and records the peak number in flight.
struct Timed {
    inner: Arc<dyn CoinbaseSource>,
    in_flight: Mutex<(usize, usize)>,
    durations: Mutex<Vec<u128>>,
}

impl CoinbaseSource for Timed {
    fn coinbase_for(&self, address: &str, prev: &str, txids: &[String]) -> Result<CoinbaseFor, RpcError> {
        {
            let mut f = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
            f.0 += 1;
            f.1 = f.1.max(f.0);
        }
        let at = Instant::now();
        let r = self.inner.coinbase_for(address, prev, txids);
        self.durations.lock().unwrap_or_else(|e| e.into_inner()).push(at.elapsed().as_millis());
        self.in_flight.lock().unwrap_or_else(|e| e.into_inner()).0 -= 1;
        r
    }
}

/// `mining.subscribe` then `mining.authorize` as `user`; the authorize answer.
async fn stratum_authorize(pool: std::net::SocketAddr, user: &str) -> Value {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (rd, mut wr) = tokio::net::TcpStream::connect(pool).await.unwrap().into_split();
    let mut lines = BufReader::new(rd).lines();
    wr.write_all(b"{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"t\",null,\"h\",\"p\"]}\n").await.unwrap();
    wr.write_all(format!("{{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"{}\",\"x\"]}}\n", user).as_bytes()).await.unwrap();
    loop {
        let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line()).await.expect("an answer").unwrap().expect("a line");
        let v: Value = serde_json::from_str(&line).unwrap();
        if v["id"] == json!(2) {
            return v;
        }
    }
}

async fn status_json(addr: std::net::SocketAddr) -> Value {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(b"GET /status HTTP/1.1\r\nHost: yolo\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut text = String::new();
    s.read_to_string(&mut text).await.unwrap();
    serde_json::from_str(text.split("\r\n\r\n").nth(1).unwrap_or("")).unwrap()
}
