// Copyright (c) 2026 The Ycash developers
// Distributed under the MIT software license, see the accompanying
// file LICENSE or https://www.opensource.org/licenses/mit-license.php .

//! End-to-end against a real node (plan §3.2.5, Y3): `cargo test --features regtest` with
//! `YCASHD` set. Starts a two-node regtest the way `stratum-perl-check` does (Yellowback on, a
//! quote set, `mineraddress=` so the template carries a tagged coinbase), runs the server
//! in-process (`yolo::bind` / `serve`) once per case, drives it with the Python `stratum-miner`
//! and checks the block on the chain:
//!
//! | case (payout × text, Y7) | assertion |
//! |---|---|
//! | username, no text | accepted, both nodes at the new height, `yed_gettag` quote at the set price, `vout[0]` pays the miner's stratum username, the node's scriptSig kept |
//! | `--payout <mineraddress>`, no text | the same with the node's address paid; the username is a worker name |
//! | username, `--text` | pays the username; scriptSig exactly height push ‖ `coinbaseaux.flags` ‖ push(text) |
//! | `--payout <miner's address>`, `--text` | pays the fixed address; scriptSig rebuilt |
//! | `--text --no-flags` | accepted but `yed_gettag` `found: false` (the Perl's behaviour, Y-F1) |
//! | `--text` of 90 bytes | scriptSig exactly 100 bytes, text truncated, tag intact |
//!
//! Environment: `YCASHD` (required; unset = skip), `STRATUM_MINER` (default: the ycash-dd
//! copy beside this repo in the workspace), `PYTHON` (default: the workspace `.venv`),
//! `YOLO_REGTEST_SCRATCH` (datadirs; default: the system temp dir), `YOLO_REGTEST_RPC_BASE`
//! / `YOLO_REGTEST_P2P_BASE` (default 26301 / 27301), `YOLO_REGTEST_POOL_NODE_ARGS`
//! (extra whitespace-separated `ycashd` arguments for node A once it serves the pool, e.g.
//! `-allowdeprecated=none` on 6.20.0 for templates without the deprecated root keys; the
//! harness's own `getnewaddress` calls run before, or on node B).
//!
//! Works against ycashd v4.5.0 (ycash-dd) and 6.20.0 (the ycash6 build: plain `ref/ycash6`
//! leaves Equihash (48,5) on regtest under these upgrades, baseline fix 3).
//!
//! **The vault upgrade** (`upgrade/vault` nodes, docs/plans/yellowback-upgrade-plan.md §15.10):
//! set `YOLO_REGTEST_VAULT=<h>` (h ≥ 104). Yellowback is then a consensus module of the network
//! upgrade `Vault` (branch id `6d5b7a31`) rather than `-yellowback`: the nodes start with
//! `-nuparams=6d5b7a31:<h>` (and without `-yellowback`/`-yellowbackstartheight`, which that node
//! refuses), yolo mines the blocks from 102 through `h + 1` before any Yellowback state exists
//! (the case `across-vault-activation`: every block accepted, the chain tip's branch id becomes
//! `6d5b7a31`), then node A creates the YED attestor set (`set_create`) and both nodes restart
//! with `-yellowbackattestorset=<setid>`, after which the six cases run as before.
#![cfg(feature = "regtest")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use yolo::equihash::{Equihash, EquihashArg};
use yolo::rpc::{RpcAuth, RpcClient};
use yolo::tag::height_push;
use yolo::Config;

const QUOTE_MICRO_USD: i64 = 50_000; // $0.05
const NODE_ARGS: &[&str] = &[
    "-nuparams=5ba81b19:1", // Overwinter
    "-nuparams=76b809bb:1", // Sapling
    "-nuparams=374d694f:1", // Ycash
    "-nuparams=8e471bd6:1", // Blossom
    "-nuparams=66314da3:1", // Heartwood
    "-nuparams=19bd2d2f:1", // Canopy
    "-experimentalfeatures",
    "-yellowback",
    "-yellowbackstartheight=1",
    "-yellowbacksigmaref=0",
];

/// The vault upgrade's consensus branch id (upgrade plan U-9).
const VAULT_BRANCH_ID: &str = "6d5b7a31";

/// `NODE_ARGS` for this run: unchanged without `YOLO_REGTEST_VAULT`; with it, the vault upgrade at
/// `h` and none of the retired Yellowback switches (finding (31): an init error on that node).
fn node_args(vault: Option<u64>) -> Vec<String> {
    let Some(h) = vault else { return NODE_ARGS.iter().map(|a| a.to_string()).collect() };
    let mut args: Vec<String> = NODE_ARGS.iter().filter(|a| **a != "-yellowback" && !a.starts_with("-yellowbackstartheight")).map(|a| a.to_string()).collect();
    args.push(format!("-nuparams={}:{}", VAULT_BRANCH_ID, h));
    args
}

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn env_or(name: &str, default: PathBuf) -> PathBuf {
    std::env::var_os(name).map(PathBuf::from).unwrap_or(default)
}

fn log(msg: &str) {
    eprintln!("[regtest] {}", msg);
}

/// One `ycashd`, killed on drop (also when an assertion panics).
struct Node {
    index: usize,
    datadir: PathBuf,
    rpc_port: u16,
    p2p_port: u16,
    connect: Option<u16>,
    rpc: RpcClient,
    /// `node_args(..)`: the network-upgrade arguments of the run.
    base_args: Vec<String>,
    child: Option<Child>,
    extra_conf: Vec<String>,
    /// Command-line arguments after `NODE_ARGS` (`-mocktime`, `YOLO_REGTEST_POOL_NODE_ARGS`).
    extra_args: Vec<String>,
}

impl Node {
    fn new(index: usize, scratch: &Path, rpc_port: u16, p2p_port: u16, connect: Option<u16>) -> Node {
        let datadir = scratch.join(format!("node{}", index));
        let _ = std::fs::remove_dir_all(&datadir);
        std::fs::create_dir_all(datadir.join("regtest")).unwrap();
        let rpc = RpcClient::new(&format!("http://127.0.0.1:{}", rpc_port), &RpcAuth { user: "u".into(), password: "p".into() });
        Node { index, datadir, rpc_port, p2p_port, connect, rpc, base_args: node_args(None), child: None, extra_conf: Vec::new(), extra_args: Vec::new() }
    }

    fn write_conf(&self) {
        let mut lines = vec![
            "regtest=1".to_string(),
            "server=1".into(),
            "rpcuser=u".into(),
            "rpcpassword=p".into(),
            format!("rpcport={}", self.rpc_port),
            format!("port={}", self.p2p_port),
            "listen=1".into(),
            "discover=0".into(),
            "keypool=1".into(),
            "rpcallowip=127.0.0.1".into(),
        ];
        if let Some(p) = self.connect {
            lines.push(format!("connect=127.0.0.1:{}", p));
        }
        lines.extend(self.extra_conf.iter().cloned());
        std::fs::write(self.datadir.join("ycash.conf"), lines.join("\n") + "\n").unwrap();
    }

    fn start(&mut self, ycashd: &Path) {
        self.write_conf();
        let stderr = std::fs::File::create(self.datadir.join("stderr.log")).unwrap();
        let child = Command::new(ycashd)
            .arg(format!("-datadir={}", self.datadir.display()))
            .args(&self.base_args)
            .args(&self.extra_args)
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .unwrap_or_else(|e| panic!("cannot start {}: {}", ycashd.display(), e));
        self.child = Some(child);
        let deadline = Instant::now() + Duration::from_secs(150);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(250));
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                panic!("node {} exited at start ({}); see {}/stderr.log", self.index, status, self.datadir.display());
            }
            if self.rpc.call("getblockcount", json!([])).is_ok() {
                return;
            }
        }
        panic!("node {} never answered RPC", self.index);
    }

    fn call(&self, method: &str, params: Value) -> Value {
        self.rpc.call(method, params).unwrap_or_else(|e| panic!("node {} {}: {}", self.index, method, e))
    }

    fn height(&self) -> u64 {
        self.call("getblockcount", json!([])).as_u64().unwrap()
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if child.try_wait().unwrap().is_none() {
                let _ = self.rpc.call("stop", json!([]));
                let deadline = Instant::now() + Duration::from_secs(30);
                while Instant::now() < deadline && child.try_wait().unwrap().is_none() {
                    std::thread::sleep(Duration::from_millis(200));
                }
                if child.try_wait().unwrap().is_none() {
                    let _ = child.kill();
                }
                let _ = child.wait();
            }
        }
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

fn wait_height(nodes: &[&Node], height: u64, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if nodes.iter().all(|n| n.height() >= height) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    false
}

struct Case {
    name: &'static str,
    /// `--payout`: the fixed address, or None (the username is paid).
    payout: Option<String>,
    /// `--text`.
    text: Option<String>,
    no_flags: bool,
    /// The stratum username: the miner's payout address without `--payout`, any name with it.
    user: String,
}

struct Outcome {
    height: u64,
    tag: Value,
    script_sig: Vec<u8>,
    vout0_addresses: Vec<String>,
}

/// Serves one case in-process, mines one block through the Python miner, returns the block's facts.
async fn run_case(case: &Case, a: &Node, b: &Node, python: &Path, miner: &Path) -> Outcome {
    let before = a.height();
    let config = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        status_bind: Some("127.0.0.1:0".parse().unwrap()),
        payout: case.payout.clone(),
        text: case.text.as_ref().map(|t| t.as_bytes().to_vec()),
        no_flags: case.no_flags,
        password: None,
        equihash: EquihashArg::Fixed(Equihash::REGTEST),
        limits: yolo::Limits::default(),
    };
    let bound = yolo::bind(a.rpc.clone(), config).await.unwrap();
    let addr = bound.addr;
    let status_addr = bound.status_addr.unwrap();
    let state = bound.state();
    let server = tokio::spawn(bound.serve());
    let deadline = Instant::now() + Duration::from_secs(20);
    while state.lock().template.is_none() {
        assert!(Instant::now() < deadline, "{}: the poller never fetched a template", case.name);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    log(&format!("{}: yolo on {} (status {}), mining as {}", case.name, addr, status_addr, case.user));

    let cmd = vec![
        miner.to_string_lossy().to_string(),
        "--pool".into(),
        addr.to_string(),
        "--user".into(),
        case.user.clone(),
        "--blocks".into(),
        "1".into(),
        "--equihash".into(),
        "48,5".into(),
        "--verbose".into(),
    ];
    let python = python.to_path_buf();
    let output = tokio::task::spawn_blocking(move || Command::new(&python).args(&cmd).output()).await.unwrap().unwrap_or_else(|e| panic!("cannot run the stratum miner: {}", e));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{}: stratum-miner exit {}\nstdout:\n{}\nstderr:\n{}", case.name, output.status, stdout, String::from_utf8_lossy(&output.stderr));
    assert!(stdout.contains("accepted job"), "{}: the miner reported no accepted job:\n{}", case.name, stdout);

    let status = reqwest_status(status_addr).await;
    log(&format!("{}: /status {}", case.name, status));
    assert_eq!(status["accepted"], json!(1), "{}: /status accepted", case.name);
    assert_eq!(status["rejected"], json!(0), "{}: /status rejected", case.name);
    assert_eq!(status["lastSubmitVerdict"], json!("accepted"), "{}: /status verdict", case.name);
    assert_eq!(status["payout"], json!(case.payout.clone().unwrap_or_else(|| "username".into())), "{}: /status payout", case.name);
    assert_eq!(status["text"], json!(case.text.is_some()), "{}: /status text", case.name);
    server.abort();

    let height = before + 1;
    assert!(wait_height(&[a, b], height, Duration::from_secs(30)), "{}: nodes did not reach {} ({} / {})", case.name, height, a.height(), b.height());
    let tag = a.call("yed_gettag", json!([height.to_string()]));
    let block = a.call("getblock", json!([height.to_string(), 2]));
    let coinbase = &block["tx"][0];
    let script_sig = hex::decode(coinbase["vin"][0]["coinbase"].as_str().unwrap()).unwrap();
    let vout0_addresses = coinbase["vout"][0]["scriptPubKey"]["addresses"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
    log(&format!("{}: height {} tag {} scriptSig {} ({} bytes) vout0 {:?}", case.name, height, tag, hex::encode(&script_sig), script_sig.len(), vout0_addresses));
    Outcome { height, tag, script_sig, vout0_addresses }
}

/// Serves yolo with no Yellowback state (no quote, no tag) and mines `blocks` blocks through the
/// Python miner in one session: the vault mode's walk across the activation height.
async fn run_plain(name: &str, a: &Node, b: &Node, python: &Path, miner: &Path, user: &str, blocks: u64) {
    let before = a.height();
    let config = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        status_bind: Some("127.0.0.1:0".parse().unwrap()),
        payout: None,
        text: None,
        no_flags: false,
        password: None,
        equihash: EquihashArg::Fixed(Equihash::REGTEST),
        limits: yolo::Limits::default(),
    };
    let bound = yolo::bind(a.rpc.clone(), config).await.unwrap();
    let addr = bound.addr;
    let status_addr = bound.status_addr.unwrap();
    let state = bound.state();
    let server = tokio::spawn(bound.serve());
    let deadline = Instant::now() + Duration::from_secs(20);
    while state.lock().template.is_none() {
        assert!(Instant::now() < deadline, "{}: the poller never fetched a template", name);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let cmd =
        vec![miner.to_string_lossy().to_string(), "--pool".into(), addr.to_string(), "--user".into(), user.to_string(), "--blocks".into(), blocks.to_string(), "--equihash".into(), "48,5".into()];
    let python = python.to_path_buf();
    let output = tokio::task::spawn_blocking(move || Command::new(&python).args(&cmd).output()).await.unwrap().unwrap_or_else(|e| panic!("cannot run the stratum miner: {}", e));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{}: stratum-miner exit {}\nstdout:\n{}\nstderr:\n{}", name, output.status, stdout, String::from_utf8_lossy(&output.stderr));
    let status = reqwest_status(status_addr).await;
    log(&format!("{}: /status {}", name, status));
    assert_eq!(status["accepted"], json!(blocks), "{}: /status accepted", name);
    assert_eq!(status["rejected"], json!(0), "{}: /status rejected", name);
    assert_eq!(status["tag"], json!("none"), "{}: no Yellowback state yet, so no tag", name);
    server.abort();
    let height = before + blocks;
    assert!(wait_height(&[a, b], height, Duration::from_secs(30)), "{}: nodes did not reach {} ({} / {})", name, height, a.height(), b.height());
}

/// `GET /status` with plain tokio (no HTTP client in the dev-dependencies).
async fn reqwest_status(addr: std::net::SocketAddr) -> Value {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(b"GET /status HTTP/1.1\r\nHost: yolo\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut text = String::new();
    s.read_to_string(&mut text).await.unwrap();
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("");
    serde_json::from_str(body).unwrap_or_else(|e| panic!("/status is not JSON ({}): {:?}", e, text))
}

fn assert_quote(name: &str, tag: &Value, payout: &str) {
    assert_eq!(tag["found"], json!(true), "{}: yed_gettag found", name);
    assert_eq!(tag["kind"], json!("quote"), "{}: yed_gettag kind", name);
    assert_eq!(tag["priceMicroUsd"], json!(QUOTE_MICRO_USD), "{}: yed_gettag price", name);
    assert_eq!(tag["payoutAddress"], json!(payout), "{}: yed_gettag payout address", name);
}

#[tokio::test(flavor = "multi_thread")]
async fn payout_text_grid_against_a_regtest_node() {
    let Some(ycashd) = std::env::var_os("YCASHD").map(PathBuf::from) else {
        eprintln!("SKIP: YCASHD is not set (point it at a ycash-dd ycashd to run the regtest integration test)");
        return;
    };
    let ws = workspace();
    let python = env_or("PYTHON", ws.join(".venv/bin/python"));
    let miner = env_or("STRATUM_MINER", ws.join("ycash-dd/contrib/yellowback/devnet/stratum-miner"));
    let scratch = env_or("YOLO_REGTEST_SCRATCH", std::env::temp_dir().join("yolo-regtest"));
    let rpc_base: u16 = std::env::var("YOLO_REGTEST_RPC_BASE").ok().and_then(|v| v.parse().ok()).unwrap_or(26301);
    let p2p_base: u16 = std::env::var("YOLO_REGTEST_P2P_BASE").ok().and_then(|v| v.parse().ok()).unwrap_or(27301);
    for (what, p) in [("YCASHD", &ycashd), ("PYTHON", &python), ("STRATUM_MINER", &miner)] {
        assert!(p.exists(), "{} = {} does not exist", what, p.display());
    }
    std::fs::create_dir_all(&scratch).unwrap();
    log(&format!("scratch {}", scratch.display()));

    let pool_node_args: Vec<String> = std::env::var("YOLO_REGTEST_POOL_NODE_ARGS").unwrap_or_default().split_whitespace().map(String::from).collect();
    let vault: Option<u64> = std::env::var("YOLO_REGTEST_VAULT").ok().map(|v| v.parse().expect("YOLO_REGTEST_VAULT is a height"));
    if let Some(h) = vault {
        assert!(h >= 104, "YOLO_REGTEST_VAULT must be at least 104 (yolo mines 102..=h+1 across it)");
        log(&format!("vault mode: -nuparams={}:{}", VAULT_BRANCH_ID, h));
    }

    // Node A mines and serves the template; node B only relays. mineraddress= must be a wallet
    // t-addr of node A (the Perl's rule), so A is started once to mint it, then restarted with it.
    let mut a = Node::new(0, &scratch, rpc_base, p2p_base, None);
    let mut b = Node::new(1, &scratch, rpc_base + 1, p2p_base + 1, Some(p2p_base));
    a.base_args = node_args(vault);
    b.base_args = node_args(vault);
    // A burst of generated blocks runs median-time-past ahead of the clock (Y-F5); generate the
    // chain an hour in the past so the pool's `max(curtime, now)` header time is accepted. The
    // first start runs on `-mocktime` rather than `setmocktime` + `setmocktime 0`: 6.20.0 refuses
    // `setmocktime` on a node started without `-mocktime`, and on one started with it `0` sets
    // the clock to the epoch instead of restoring the system clock (F-8). The restart below
    // drops `-mocktime`, so the node mines on the system clock; B starts after and syncs.
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    a.extra_args.push(format!("-mocktime={}", now - 3600));
    a.start(&ycashd);
    let mineraddress = a.call("getnewaddress", json!([])).as_str().unwrap().to_string();
    a.call("generate", json!([101]));
    a.stop();
    a.extra_args = pool_node_args;
    a.extra_conf.push(format!("mineraddress={}", mineraddress));
    a.start(&ycashd);
    assert_eq!(a.height(), 101, "node A kept its chain across the restart");
    b.start(&ycashd);
    assert!(wait_height(&[&a, &b], 101, Duration::from_secs(60)), "node B did not sync the initial chain");
    if let Some(h) = vault {
        // Across the activation through yolo: templates before, at and after the upgrade height.
        let info = a.call("getblockchaininfo", json!([]));
        assert_ne!(info["consensus"]["nextblock"], json!(VAULT_BRANCH_ID), "the upgrade is not active at 102");
        let user = b.call("getnewaddress", json!([])).as_str().unwrap().to_string();
        run_plain("across-vault-activation", &a, &b, &python, &miner, &user, h + 1 - a.height()).await;
        for node in [&a, &b] {
            let info = node.call("getblockchaininfo", json!([]));
            assert_eq!(info["consensus"]["chaintip"], json!(VAULT_BRANCH_ID), "node {}: the tip is past the vault activation", node.index);
            assert_eq!(info["upgrades"][VAULT_BRANCH_ID]["status"], json!("active"), "node {}: Vault active", node.index);
            assert_eq!(info["upgrades"][VAULT_BRANCH_ID]["activationheight"], json!(h), "node {}: Vault height", node.index);
        }
        log(&format!("across-vault-activation: yolo mined 102..={} through the upgrade at {}; tip branch {}", h + 1, h, VAULT_BRANCH_ID));
        // The YED attestor set (U-22): created after activation, then both nodes restart naming it.
        let created = a.call("set_create", json!([{"seats": 15, "unlockthreshold": 1, "cancelthreshold": 1, "slashthreshold": 1, "open": true, "maturity": 1}]));
        let setid = created["setid"].as_str().unwrap().to_string();
        let deadline = Instant::now() + Duration::from_secs(30);
        while a.call("getrawmempool", json!([])).as_array().map(|m| m.is_empty()).unwrap_or(true) {
            assert!(Instant::now() < deadline, "set_create never reached the mempool");
            std::thread::sleep(Duration::from_millis(200));
        }
        a.call("generate", json!([1]));
        let height = a.height();
        assert!(wait_height(&[&a, &b], height, Duration::from_secs(30)), "node B did not sync the set");
        for node in [&mut a, &mut b] {
            node.stop();
            node.extra_args.push(format!("-yellowbackattestorset={}", setid));
        }
        a.start(&ycashd);
        b.start(&ycashd);
        assert!(wait_height(&[&a, &b], height, Duration::from_secs(60)), "the nodes did not come back at {}", height);
        log(&format!("YED attestor set {} at {}; nodes restarted with it", setid, height));
    }
    a.call("yed_setquote", json!([QUOTE_MICRO_USD, 1]));
    let template = a.call("getblocktemplate", json!([]));
    let flags = hex::decode(template["coinbaseaux"]["flags"].as_str().unwrap_or("")).unwrap();
    assert_eq!(flags.len(), 37, "the template carries a 37-byte quote tag: {:?}", template["coinbaseaux"]);
    let miner_addr = b.call("getnewaddress", json!([])).as_str().unwrap().to_string();
    log(&format!("chain at {}; mineraddress {} miner {} flags {}", a.height(), mineraddress, miner_addr, hex::encode(&flags)));
    log(&format!("template header roots: lightclientroothash {} defaultroots {}", template["lightclientroothash"], template["defaultroots"]));

    let text = "yolo regtest";
    let long_text = "x".repeat(90);
    let case = |name, payout: Option<&String>, text: Option<&str>, no_flags, user: &str| Case { name, payout: payout.cloned(), text: text.map(String::from), no_flags, user: user.to_string() };
    let cases = [
        case("username", None, None, false, &miner_addr),
        case("fixed", Some(&mineraddress), None, false, "stratum-miner"),
        case("username-text", None, Some(text), false, &miner_addr),
        case("fixed-text", Some(&miner_addr), Some(text), false, "worker.1"),
        case("text-no-flags", None, Some(text), true, &miner_addr),
        case("text-90", None, Some(&long_text), false, &miner_addr),
    ];
    for case in &cases {
        let o = run_case(case, &a, &b, &python, &miner).await;
        let hp = height_push(o.height as u32);
        let rebuilt = |t: &str| {
            let mut want = hp.clone();
            want.extend_from_slice(&flags);
            want.push(t.len() as u8);
            want.extend_from_slice(t.as_bytes());
            want
        };
        match case.name {
            "username" => {
                assert_quote(case.name, &o.tag, &mineraddress);
                assert_eq!(o.vout0_addresses, vec![miner_addr.clone()], "no --payout pays the miner's username");
                assert!(o.script_sig.starts_with(&hp) && o.script_sig.ends_with(&flags), "no --text keeps the node's scriptSig");
            }
            "fixed" => {
                assert_quote(case.name, &o.tag, &mineraddress);
                assert_eq!(o.vout0_addresses, vec![mineraddress.clone()], "--payout pays the fixed address");
                assert!(o.script_sig.starts_with(&hp) && o.script_sig.ends_with(&flags), "no --text keeps the node's scriptSig");
            }
            "username-text" => {
                assert_quote(case.name, &o.tag, &mineraddress);
                assert_eq!(o.vout0_addresses, vec![miner_addr.clone()], "no --payout pays the miner's username");
                assert_eq!(o.script_sig, rebuilt(text), "--text scriptSig = height push ‖ flags ‖ push(text)");
            }
            "fixed-text" => {
                assert_quote(case.name, &o.tag, &mineraddress);
                assert_eq!(o.vout0_addresses, vec![miner_addr.clone()], "--payout pays the fixed address, whatever the username");
                assert_eq!(o.script_sig, rebuilt(text), "--text scriptSig = height push ‖ flags ‖ push(text)");
            }
            "text-no-flags" => {
                assert_eq!(o.tag["found"], json!(false), "--text --no-flags drops the tag (Y-F1): {}", o.tag);
                let mut want = hp.clone();
                want.push(text.len() as u8);
                want.extend_from_slice(text.as_bytes());
                assert_eq!(o.script_sig, want, "--no-flags scriptSig = height push ‖ push(text)");
            }
            "text-90" => {
                assert_quote(case.name, &o.tag, &mineraddress);
                assert_eq!(o.script_sig.len(), 100, "--text at the scriptSig limit: exactly 100 bytes");
                let fixed = hp.len() + flags.len();
                let used = 100 - fixed - 1;
                assert_eq!(o.script_sig, rebuilt(&long_text[..used]), "--text is truncated to fit, tag intact");
            }
            _ => unreachable!(),
        }
        log(&format!("{}: ok", case.name));
    }

    a.stop();
    b.stop();
}
