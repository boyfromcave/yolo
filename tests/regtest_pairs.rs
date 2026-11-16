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
//! 3. `-ydf=5` on P (and W): the three kinds again; each block pays 95 % of the subsidy plus all
//!    fees, and its fund output equals the stock coinbase's.
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
use yolo::rpc::{CoinbaseFor, CoinbaseSource, RpcAuth, RpcClient, RpcError};
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
            let have: BTreeSet<String> = t["transactions"].as_array().unwrap().iter().map(|x| x["hash"].as_str().unwrap().to_string()).collect();
            let have: BTreeSet<&str> = have.iter().map(String::as_str).collect();
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

impl CoinbaseSource for Stub {
    fn coinbase_for(&self, address: &str, prev: &str, txids: &[String]) -> Result<CoinbaseFor, RpcError> {
        let (t, data, addresstype) = if address == self.sapling {
            let t = Stub::template_for(&self.helper, prev, txids)?;
            let data = hex::decode(t["coinbasetxn"]["data"].as_str().unwrap()).unwrap();
            (t, data, "sapling")
        } else {
            let t = Stub::template_for(&self.pool, prev, txids)?;
            let v = self.pool.validateaddress(address)?;
            if v.pqscheme.is_none() {
                return Err(RpcError::Node { code: -5, message: "Address type cannot receive a coinbase: stub".into() });
            }
            let mut cb = Coinbase::parse(&hex::decode(t["coinbasetxn"]["data"].as_str().unwrap()).unwrap()).unwrap();
            cb.vout[0].script_pubkey = hex::decode(v.script_pubkey.unwrap()).unwrap();
            (t, cb.serialize(), "pq")
        };
        let cb = Coinbase::parse(&data).unwrap();
        let fees: u64 = t["transactions"].as_array().unwrap().iter().map(|x| x["fee"].as_u64().unwrap()).sum();
        let claimed = cb.claimed_total().unwrap();
        let fund = if addresstype == "sapling" { cb.transparent_total().unwrap() } else { cb.vout[1..].iter().map(|o| o.value).sum() };
        let height = t["height"].as_u64().unwrap() as u32;
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
            subsidy: claimed - fees,
            fund,
            fundrule: fundrule.into(),
            fees,
            amount: claimed - fund,
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
        *self.asked.lock().unwrap().entry(address.to_string()).or_insert(0) += 1;
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

    /// The fees of a block's transactions, from P's wallet (it sent all of them).
    /// The block subsidy at `height`, from the node's `getblocksubsidy` (`miner` plus `founders`:
    /// both lines answer both keys), not from any coinbase the code under test handled.
    fn subsidy_at(&self, height: u64) -> u64 {
        let s = self.p.call("getblocksubsidy", json!([height]));
        zat(&s["miner"]) + zat(&s["founders"])
    }

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
    let config = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        status_bind: Some("127.0.0.1:0".parse().unwrap()),
        payout: None,
        text: None,
        no_flags: false,
        password: None,
        equihash: EquihashArg::Fixed(Equihash::REGTEST),
        limits: yolo::Limits::default(),
        test_coinbase_source: Some(source as Arc<dyn CoinbaseSource>),
    };
    let bound = yolo::bind(ctx.p.rpc.clone(), config).await.unwrap();
    let (addr, status, state) = (bound.addr, bound.status_addr.unwrap(), bound.state());
    let server = tokio::spawn(bound.serve());
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

    // 3. -ydf=5 on the pool node (D-2): both paths pay 95 % of the subsidy plus all fees
    ctx.p.stop();
    ctx.w.stop();
    start(&mut ctx.p, &mut ctx.w, 5);
    sequential(&ctx, &miners[..3], 5, make_source(&ctx.p, &ctx.w, 5)).await;
    log("phase 3 (-ydf=5): ok");
    ctx.p.stop();
    ctx.w.stop();
}

async fn status_json(addr: std::net::SocketAddr) -> Value {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(b"GET /status HTTP/1.1\r\nHost: yolo\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut text = String::new();
    s.read_to_string(&mut text).await.unwrap();
    serde_json::from_str(text.split("\r\n\r\n").nth(1).unwrap_or("")).unwrap()
}
