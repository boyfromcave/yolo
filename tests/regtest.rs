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
//! / `YOLO_REGTEST_P2P_BASE` (default 26301 / 27301).
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
    child: Option<Child>,
    extra_conf: Vec<String>,
}

impl Node {
    fn new(index: usize, scratch: &Path, rpc_port: u16, p2p_port: u16, connect: Option<u16>) -> Node {
        let datadir = scratch.join(format!("node{}", index));
        let _ = std::fs::remove_dir_all(&datadir);
        std::fs::create_dir_all(datadir.join("regtest")).unwrap();
        let rpc = RpcClient::new(&format!("http://127.0.0.1:{}", rpc_port), &RpcAuth { user: "u".into(), password: "p".into() });
        Node { index, datadir, rpc_port, p2p_port, connect, rpc, child: None, extra_conf: Vec::new() }
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
            .args(NODE_ARGS)
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
    let output = tokio::task::spawn_blocking(move || Command::new(&python).args(&cmd).output())
        .await
        .unwrap()
        .unwrap_or_else(|e| panic!("cannot run the stratum miner: {}", e));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}: stratum-miner exit {}\nstdout:\n{}\nstderr:\n{}",
        case.name,
        output.status,
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
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
    let vout0_addresses = coinbase["vout"][0]["scriptPubKey"]["addresses"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    log(&format!("{}: height {} tag {} scriptSig {} ({} bytes) vout0 {:?}", case.name, height, tag, hex::encode(&script_sig), script_sig.len(), vout0_addresses));
    Outcome { height, tag, script_sig, vout0_addresses }
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

    // Node A mines and serves the template; node B only relays. mineraddress= must be a wallet
    // t-addr of node A (the Perl's rule), so A is started once to mint it, then restarted with it.
    let mut a = Node::new(0, &scratch, rpc_base, p2p_base, None);
    let mut b = Node::new(1, &scratch, rpc_base + 1, p2p_base + 1, Some(p2p_base));
    a.start(&ycashd);
    let mineraddress = a.call("getnewaddress", json!([])).as_str().unwrap().to_string();
    a.stop();
    a.extra_conf.push(format!("mineraddress={}", mineraddress));
    a.start(&ycashd);
    b.start(&ycashd);
    // A burst of generated blocks runs median-time-past ahead of the clock (Y-F5); generate the
    // chain an hour in the past so the pool's `max(curtime, now)` header time is accepted.
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    a.call("setmocktime", json!([now - 3600]));
    a.call("generate", json!([101]));
    a.call("setmocktime", json!([0]));
    assert!(wait_height(&[&a, &b], 101, Duration::from_secs(60)), "node B did not sync the initial chain");
    a.call("yed_setquote", json!([QUOTE_MICRO_USD, 1]));
    let template = a.call("getblocktemplate", json!([]));
    let flags = hex::decode(template["coinbaseaux"]["flags"].as_str().unwrap_or("")).unwrap();
    assert_eq!(flags.len(), 37, "the template carries a 37-byte quote tag: {:?}", template["coinbaseaux"]);
    let miner_addr = b.call("getnewaddress", json!([])).as_str().unwrap().to_string();
    log(&format!("chain at 101; mineraddress {} miner {} flags {}", mineraddress, miner_addr, hex::encode(&flags)));

    let text = "yolo regtest";
    let long_text = "x".repeat(90);
    let case = |name, payout: Option<&String>, text: Option<&str>, no_flags, user: &str| Case {
        name,
        payout: payout.cloned(),
        text: text.map(String::from),
        no_flags,
        user: user.to_string(),
    };
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
