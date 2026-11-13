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
//! **The vault upgrade** (`upgrade/vault` nodes, docs/plans/yellowback-upgrade-plan.md §15.10) is
//! the default on this branch: the upgrade activates at `YOLO_REGTEST_VAULT=<h>` (h ≥ 104,
//! default 110). Yellowback is a consensus module of the network upgrade `Vault` (branch id
//! `6d5b7a31`) rather than `-yellowback`: the nodes start with
//! `-nuparams=6d5b7a31:<h>` (and without `-yellowback`/`-yellowbackstartheight`, which that node
//! refuses), yolo mines the blocks from 102 through `h + 1` before any Yellowback state exists
//! (the case `across-vault-activation`: every block accepted, the chain tip's branch id becomes
//! `6d5b7a31`), then node A creates the YED attestor set (`set_create`) and both nodes restart
//! with `-yellowbackattestorset=<setid>`, after which the six cases run as before.
//! `YOLO_REGTEST_LEGACY=1` runs against a node without the vault upgrade (`harden/yellowback`)
//! instead: `NODE_ARGS` as they are, with `-experimentalfeatures -yellowback`.
//!
//! **The kogany mode** (`YOLO_REGTEST_KOGANY=<koganyd built with --features ycash>`, vault mode
//! only; kogany plan W8-c): a kogany node (`Kogany`) joins as a third node, dialling node A over
//! P2P, and yolo serves *its* templates; nodes A and B stay ycashd and keep the wallet roles
//! (`getnewaddress`, `generate`, `set_create`; kogany is wallet-less, plan K-3). kogany never
//! solves Equihash (K-16), so every block is solved by the Python miner and must reach both
//! ycashd nodes, which verify (48,5). The block facts (`yed_gettag`, `getblock`) are read from
//! node A. Until plan W5-f kogany's Yellowback state is its mock (`KOGANY_YED_RPC_MOCK=1`): the
//! tag carries the mock's fixture payout key, which `yed_setquote` reports, instead of
//! `mineraddress`'s. Only the test layer differs; yolo's code is unchanged.
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

/// The default vault activation height (the CI's `YOLO_REGTEST_VAULT` on upgrade/vault).
const DEFAULT_VAULT_HEIGHT: u64 = 110;

/// `NODE_ARGS` for this run: unchanged under `YOLO_REGTEST_LEGACY`; otherwise the vault upgrade at
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

/// What the cases ask of a node, a ycashd [`Node`] or a kogany [`Kogany`]: its RPC.
trait Rpc {
    fn client(&self) -> &RpcClient;
    fn label(&self) -> String;

    fn call(&self, method: &str, params: Value) -> Value {
        self.client().call(method, params).unwrap_or_else(|e| panic!("{} {}: {}", self.label(), method, e))
    }

    fn height(&self) -> u64 {
        self.call("getblockcount", json!([])).as_u64().unwrap()
    }
}

impl Rpc for Node {
    fn client(&self) -> &RpcClient {
        &self.rpc
    }

    fn label(&self) -> String {
        format!("node {}", self.index)
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

/// The kogany mode's pool node (`YOLO_REGTEST_KOGANY=<koganyd>`): a `koganyd --features ycash`
/// on the same Ycash regtest, wallet-less (kogany plan K-3), dialling ycashd node B over P2P. It
/// validates Equihash (48,5) by shape only and never solves it (K-16): the Python miner solves its
/// templates and node B, which verifies the proof of work, must accept every block. Until plan
/// W5-f registers the YED module, its Yellowback state is `MockYedState`
/// (`KOGANY_YED_RPC_MOCK=1`), so `yed_setquote` sets the tag and the tag's payout key is the
/// mock's fixture key, not `mineraddress`. Killed on drop.
struct Kogany {
    rpc: RpcClient,
    datadir: PathBuf,
    p2p_port: u16,
    child: Child,
}

impl Kogany {
    fn start(koganyd: &Path, scratch: &Path, rpc_port: u16, p2p_port: u16, peer_p2p_port: u16, vault: u64, miner_address: &str) -> Kogany {
        let datadir = scratch.join("kogany");
        let _ = std::fs::remove_dir_all(&datadir);
        std::fs::create_dir_all(&datadir).unwrap();
        let root = datadir.display();
        let heights = ["Overwinter", "Sapling", "Ycash", "Blossom", "Heartwood", "Canopy"].iter().map(|k| format!("{} = 1\n", k)).collect::<String>();
        let toml = format!(
            "[network]\nlisten_addr = \"127.0.0.1:{p2p}\"\ninitial_testnet_peers = [\"127.0.0.1:{peer}\"]\np2p_stack = \"legacy\"\nmax_connections_per_ip = 1\n\
             cache_dir = \"{root}/network\"\nidentity_dir = \"{root}/identity\"\n\n[network.network]\nregtest = true\n\n\
             [network.network.params.activation_heights]\n{heights}Vault = {vault}\n\n[network.zakura]\nlisten_addr = \"127.0.0.1:{v2}\"\n\n\
             [rpc]\nlisten_addr = \"127.0.0.1:{rpc}\"\nenable_cookie_auth = false\ncookie_dir = \"{root}/rpc\"\n\n\
             [state]\ncache_dir = \"{root}/state\"\nephemeral = false\n\n[mempool]\ndebug_enable_at_height = 0\n\n\
             [mining]\ninternal_miner = false\nminer_address = \"{miner_address}\"\n\n[tracing]\nuse_color = false\nfilter = \"info\"\n",
            p2p = p2p_port,
            peer = peer_p2p_port,
            v2 = p2p_port + 100,
            rpc = rpc_port,
        );
        let conf = datadir.join("koganyd.toml");
        std::fs::write(&conf, toml).unwrap();
        let log = std::fs::File::create(datadir.join("koganyd.log")).unwrap();
        let child = Command::new(koganyd)
            .arg("-c")
            .arg(&conf)
            .arg("start")
            .env("KOGANY_YED_RPC_MOCK", "1")
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap_or_else(|e| panic!("cannot start {}: {}", koganyd.display(), e));
        let rpc = RpcClient::new(&format!("http://127.0.0.1:{}", rpc_port), &RpcAuth { user: "u".into(), password: "p".into() });
        let mut k = Kogany { rpc, datadir, p2p_port, child };
        let deadline = Instant::now() + Duration::from_secs(150);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(250));
            if let Some(status) = k.child.try_wait().unwrap() {
                panic!("kogany exited at start ({}); see {}/koganyd.log", status, k.datadir.display());
            }
            if k.rpc.call("getblockcount", json!([])).is_ok() {
                return k;
            }
        }
        panic!("kogany never answered RPC; see {}/koganyd.log", k.datadir.display());
    }

    /// Wait until kogany lists a peer, its only one being `b` (kogany dials it; after `b`
    /// restarts, `b` is asked to dial kogany back). `b`'s own list would not do: it holds node B.
    fn wait_peer(&self, b: &Node) {
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut asked = Instant::now();
        while self.call("getpeerinfo", json!([])).as_array().is_none_or(|p| p.is_empty()) {
            assert!(Instant::now() < deadline, "kogany and node B never connected; see {}/koganyd.log", self.datadir.display());
            if asked.elapsed() > Duration::from_secs(5) {
                let _ = b.rpc.call("addnode", json!([format!("127.0.0.1:{}", self.p2p_port), "onetry"]));
                asked = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }
}

impl Rpc for Kogany {
    fn client(&self) -> &RpcClient {
        &self.rpc
    }

    fn label(&self) -> String {
        "kogany".into()
    }
}

impl Drop for Kogany {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_height(nodes: &[&dyn Rpc], height: u64, timeout: Duration) -> bool {
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

/// Serves one case in-process from `a`'s templates, mines one block through the Python miner,
/// returns the block's facts as `facts` reports them (`a`, or node B when `a` is kogany).
async fn run_case(case: &Case, a: &dyn Rpc, b: &dyn Rpc, facts: &dyn Rpc, python: &Path, miner: &Path) -> Outcome {
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
    let bound = yolo::bind(a.client().clone(), config).await.unwrap();
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
    assert!(wait_height(&[a, b, facts], height, Duration::from_secs(30)), "{}: nodes did not reach {} ({} / {} / {})", case.name, height, a.height(), b.height(), facts.height());
    let tag = facts.call("yed_gettag", json!([height.to_string()]));
    let block = facts.call("getblock", json!([height.to_string(), 2]));
    let coinbase = &block["tx"][0];
    let script_sig = hex::decode(coinbase["vin"][0]["coinbase"].as_str().unwrap()).unwrap();
    let vout0_addresses = coinbase["vout"][0]["scriptPubKey"]["addresses"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
    log(&format!("{}: height {} tag {} scriptSig {} ({} bytes) vout0 {:?}", case.name, height, tag, hex::encode(&script_sig), script_sig.len(), vout0_addresses));
    Outcome { height, tag, script_sig, vout0_addresses }
}

/// Serves yolo with no Yellowback state (no quote, no tag) and mines `blocks` blocks through the
/// Python miner in one session: the vault mode's walk across the activation height.
async fn run_plain(name: &str, a: &dyn Rpc, b: &dyn Rpc, python: &Path, miner: &Path, user: &str, blocks: u64) {
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
    let bound = yolo::bind(a.client().clone(), config).await.unwrap();
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
    let legacy = std::env::var("YOLO_REGTEST_LEGACY").is_ok_and(|v| !v.is_empty() && v != "0");
    let vault: Option<u64> = if legacy { None } else { Some(std::env::var("YOLO_REGTEST_VAULT").ok().map_or(DEFAULT_VAULT_HEIGHT, |v| v.parse().expect("YOLO_REGTEST_VAULT is a height"))) };
    let kogany_bin = std::env::var_os("YOLO_REGTEST_KOGANY").filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(z) = &kogany_bin {
        assert!(z.exists(), "YOLO_REGTEST_KOGANY = {} does not exist", z.display());
        assert!(vault.is_some(), "YOLO_REGTEST_KOGANY runs in the vault mode only (kogany has no -yellowback switch)");
        log(&format!("kogany mode: the pool node is {}", z.display()));
    }
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
    // chain an hour in the past so the node's `curtime` (the pool's header time) stays within its rules. The
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
    // The kogany mode: kogany joins as a third node, dialling A, and serves the pool; A and B stay
    // the wallet (mineraddress, set_create, generate) and the C++ check of every block kogany
    // builds and yolo's miner solves (kogany plan W8-c). A's 101 blocks reach kogany by submitblock.
    let kogany = kogany_bin.as_ref().map(|z| {
        let k = Kogany::start(z, &scratch, rpc_base + 2, p2p_base + 2, p2p_base, vault.unwrap(), &mineraddress);
        for height in 1..=101u64 {
            let hash = a.call("getblockhash", json!([height]));
            let raw = a.call("getblock", json!([hash, 0]));
            let verdict = k.call("submitblock", json!([raw]));
            assert!(verdict.is_null() || verdict == json!("duplicate"), "kogany refused block {}: {}", height, verdict);
        }
        k.wait_peer(&a);
        assert!(wait_height(&[&k], 101, Duration::from_secs(60)), "kogany did not take the initial chain");
        k
    });
    // The node yolo serves: kogany in the kogany mode, else node A.
    fn pool<'x>(kogany: &'x Option<Kogany>, a: &'x Node) -> &'x dyn Rpc {
        match kogany {
            Some(k) => k,
            None => a,
        }
    }
    if let Some(h) = vault {
        // Across the activation through yolo: templates before, at and after the upgrade height.
        let info = pool(&kogany, &a).call("getblockchaininfo", json!([]));
        assert_ne!(info["consensus"]["nextblock"], json!(VAULT_BRANCH_ID), "the upgrade is not active at 102");
        let user = b.call("getnewaddress", json!([])).as_str().unwrap().to_string();
        run_plain("across-vault-activation", pool(&kogany, &a), &b, &python, &miner, &user, h + 1 - a.height()).await;
        if let Some(k) = &kogany {
            assert!(wait_height(&[&a], h + 1, Duration::from_secs(30)), "node A did not take kogany's blocks");
            let info = k.call("getblockchaininfo", json!([]));
            assert_eq!(info["consensus"]["chaintip"], json!(VAULT_BRANCH_ID), "kogany: the tip is past the vault activation");
            assert_eq!(info["upgrades"][VAULT_BRANCH_ID]["activationheight"], json!(h), "kogany: Vault height");
            assert_eq!(k.call("getbestblockhash", json!([])), b.call("getbestblockhash", json!([])), "kogany and node B share the tip");
        }
        for node in [&a, &b] {
            let info = node.call("getblockchaininfo", json!([]));
            assert_eq!(info["consensus"]["chaintip"], json!(VAULT_BRANCH_ID), "node {}: the tip is past the vault activation", node.index);
            assert_eq!(info["upgrades"][VAULT_BRANCH_ID]["status"], json!("active"), "node {}: Vault active", node.index);
            assert_eq!(info["upgrades"][VAULT_BRANCH_ID]["activationheight"], json!(h), "node {}: Vault height", node.index);
        }
        log(&format!("across-vault-activation: yolo mined 102..={} through the upgrade at {}; tip branch {}", h + 1, h, VAULT_BRANCH_ID));
        // The YED attestor set (U-22): created after activation, then both nodes restart naming it.
        // maturity 2: the post-quantum nodes refuse less than MIN_SET_MATURITY (regtest 2, mainnet 16,128;
        // D-Q-31, set_create's "maturity must be at least"); the pre-quantum upgrade/vault nodes take it too.
        let created = a.call("set_create", json!([{"seats": 15, "unlockthreshold": 1, "cancelthreshold": 1, "slashthreshold": 1, "open": true, "maturity": 2}]));
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
        if let Some(k) = &kogany {
            k.wait_peer(&a);
            assert!(wait_height(&[k], height, Duration::from_secs(60)), "kogany did not follow node A to {}", height);
        }
        log(&format!("YED attestor set {} at {}; nodes restarted with it", setid, height));
    }
    let ack = pool(&kogany, &a).call("yed_setquote", json!([QUOTE_MICRO_USD, 1]));
    // The tag's payout key: mineraddress's on ycashd; on kogany the mock's fixture key until W5-f,
    // which yed_setquote reports as nextTag.payoutAddress.
    let tag_payout = if kogany.is_some() { ack["nextTag"]["payoutAddress"].as_str().expect("yed_setquote nextTag.payoutAddress").to_string() } else { mineraddress.clone() };
    let template = pool(&kogany, &a).call("getblocktemplate", json!([]));
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
        let o = run_case(case, pool(&kogany, &a), &b, &a, &python, &miner).await;
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
                assert_quote(case.name, &o.tag, &tag_payout);
                assert_eq!(o.vout0_addresses, vec![miner_addr.clone()], "no --payout pays the miner's username");
                assert!(o.script_sig.starts_with(&hp) && o.script_sig.ends_with(&flags), "no --text keeps the node's scriptSig");
            }
            "fixed" => {
                assert_quote(case.name, &o.tag, &tag_payout);
                assert_eq!(o.vout0_addresses, vec![mineraddress.clone()], "--payout pays the fixed address");
                assert!(o.script_sig.starts_with(&hp) && o.script_sig.ends_with(&flags), "no --text keeps the node's scriptSig");
            }
            "username-text" => {
                assert_quote(case.name, &o.tag, &tag_payout);
                assert_eq!(o.vout0_addresses, vec![miner_addr.clone()], "no --payout pays the miner's username");
                assert_eq!(o.script_sig, rebuilt(text), "--text scriptSig = height push ‖ flags ‖ push(text)");
            }
            "fixed-text" => {
                assert_quote(case.name, &o.tag, &tag_payout);
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
                assert_quote(case.name, &o.tag, &tag_payout);
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
