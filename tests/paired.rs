// Copyright (c) 2026 The Ycash developers
// Distributed under the MIT software license, see the accompanying
// file LICENSE or https://www.opensource.org/licenses/mit-license.php .

//! Paired jobs end to end in-process (beta3 item 3, `docs/plans/beta3/getcoinbasetemplatefor.md`
//! §8): a fake node serves the template (real regtest vectors), `validateaddress`,
//! `z_validateaddress` and `submitblock`; a fake [`CoinbaseSource`] stands in for
//! `getcoinbasetemplatefor` and counts what it is asked. The Sapling answer is the coinbase a real
//! node built for the same tip and transactions (`regtest-template-111-sapling.json`), the
//! post-quantum one the pool node's coinbase with the miner's script at `vout[0]` (note §8.4).

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use yolo::codec::dsha256;
use yolo::equihash::{Equihash, EquihashArg};
use yolo::rpc::{CoinbaseFor, CoinbaseSource, RpcAuth, RpcClient, RpcError};
use yolo::tx::Coinbase;
use yolo::Config;

const POOL_TEMPLATE: &str = include_str!("vectors/regtest-template-111-nofund.json");
const HELPER_TEMPLATE: &str = include_str!("vectors/regtest-template-111-sapling.json");
const T_ADDR: &str = "smbUNh2QyqK15JymZ4gqKPeBncHGn2hn9vS";
const T_SPK: &str = "76a914000102030405060708090a0b0c0d0e0f1011121388ac";
const Z_ADDR: &str = "yregtestsapling1sqvnkwrm8ll82v8v7lertpt879usgxqflr9f00px6yyz9wzjqazxdl2sejlgujmvwpcxqxld46n";
const PQ_ADDR: &str = "rqfAbcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ1";

fn pq_spk(address: &str) -> Vec<u8> {
    let mut h = dsha256(address.as_bytes()).to_vec();
    let mut s = vec![0x20];
    s.append(&mut h);
    s.extend_from_slice(&[0x52, 0xc2]);
    s
}

/// The fake node: JSON-RPC over HTTP/1.1. `getcoinbasetemplatefor` is "Method not found" (only the
/// test source answers it).
async fn fake_node(template: Arc<Mutex<Value>>, submitted: Arc<Mutex<Vec<String>>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { break };
            let (template, submitted) = (template.clone(), submitted.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                loop {
                    let n = match socket.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    let Some(idx) = text.find("\r\n\r\n") else { continue };
                    let len: usize = text[..idx].lines().find_map(|h| h.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap())).unwrap_or(0);
                    if buf.len() < idx + 4 + len {
                        continue;
                    }
                    let body: Value = serde_json::from_slice(&buf[idx + 4..idx + 4 + len]).unwrap();
                    let params = body["params"].as_array().cloned().unwrap_or_default();
                    let a = params.first().and_then(Value::as_str).unwrap_or("").to_string();
                    let mut error = Value::Null;
                    // Test switches, keys of the template object: `x-test-down` (getblocktemplate
                    // gets no answer), `x-test-submit-fail-once` (the next submitblock gets no
                    // answer and is not recorded), `x-test-submit-delay-ms` (submitblock is slow).
                    let method = body["method"].as_str().unwrap_or("").to_string();
                    let switch = |k: &str| template.lock().unwrap().get(k).cloned().unwrap_or(Value::Null);
                    if method == "getblocktemplate" && switch("x-test-down") == json!(true) {
                        return;
                    }
                    if method == "submitblock" {
                        if switch("x-test-submit-fail-once") == json!(true) {
                            template.lock().unwrap().as_object_mut().unwrap().remove("x-test-submit-fail-once");
                            return;
                        }
                        if let Some(ms) = switch("x-test-submit-delay-ms").as_u64() {
                            tokio::time::sleep(Duration::from_millis(ms)).await;
                        }
                    }
                    let result = match method.as_str() {
                        // `x-test-chain` in the template object: the chain this fake node says it is
                        "getblockchaininfo" => json!({ "chain": template.lock().unwrap().get("x-test-chain").and_then(Value::as_str).unwrap_or("regtest"), "blocks": 110 }),
                        "getblocktemplate" => template.lock().unwrap().clone(),
                        "submitblock" => {
                            // the chain moves on: a new parent and header root, the same height (the
                            // vectors' coinbases carry the height push for 111)
                            let n = {
                                let mut s = submitted.lock().unwrap();
                                s.push(a);
                                s.len()
                            };
                            let mut t = template.lock().unwrap();
                            let o = t.as_object_mut().unwrap();
                            o.insert("previousblockhash".into(), json!(format!("{:062x}aa", n)));
                            for k in ["lightclientroothash", "finalsaplingroothash", "blockcommitmentshash"] {
                                if o.contains_key(k) {
                                    o.insert(k.into(), json!(format!("{:062x}bb", n)));
                                }
                            }
                            if let Some(r) = o.get_mut("defaultroots").and_then(Value::as_object_mut) {
                                r.insert("chainhistoryroot".into(), json!(format!("{:062x}bb", n)));
                            }
                            Value::Null
                        }
                        "validateaddress" if a == T_ADDR => json!({ "isvalid": true, "scriptPubKey": T_SPK }),
                        "validateaddress" if a.starts_with("rq") && a.len() == 53 => json!({ "isvalid": true, "scriptPubKey": hex::encode(pq_spk(&a)), "pqscheme": 2 }),
                        "validateaddress" => json!({ "isvalid": false }),
                        "z_validateaddress" if a.starts_with("yregtestsapling1") => json!({ "isvalid": true, "address_type": "sapling" }),
                        "z_validateaddress" => json!({ "isvalid": false }),
                        _ => {
                            error = json!({ "code": -32601, "message": "Method not found" });
                            Value::Null
                        }
                    };
                    let reply = json!({ "result": result, "error": error, "id": body["id"] }).to_string();
                    let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", reply.len(), reply);
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                    return;
                }
            });
        }
    });
    addr
}

/// What the fake source does on its next answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Good,
    /// Answers `template-stale` once, then good answers.
    StaleOnce,
    /// A fee total one zatoshi short (a §8.3 check refuses it).
    WrongFees,
    /// "Method not found", as a node without the call answers.
    NoMethod,
    /// The node's `coinbase-does-not-fit` refusal (code -1, the string both C++ lines return).
    DoesNotFit,
}

struct FakeSource {
    template: Arc<Mutex<Value>>,
    mode: Mutex<Mode>,
    calls: Mutex<Vec<(String, String, Vec<String>)>>,
    delay: Duration,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    /// What a probe (`has_call`) finds.
    call: AtomicBool,
}

impl FakeSource {
    fn new(template: Arc<Mutex<Value>>, delay: Duration) -> Arc<FakeSource> {
        Arc::new(FakeSource {
            template,
            mode: Mutex::new(Mode::Good),
            calls: Mutex::new(Vec::new()),
            delay,
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            call: AtomicBool::new(true),
        })
    }
    fn calls(&self) -> Vec<(String, String, Vec<String>)> {
        self.calls.lock().unwrap().clone()
    }
}

/// A malformed fixture, as an error of the fake call (the fakes return `Result`, so they do not panic).
fn fixture(what: impl std::fmt::Display) -> RpcError {
    RpcError::Protocol(format!("test fixture: {}", what))
}

fn answer(t: &Value, address: &str, prev: &str, txids: &[String]) -> Result<CoinbaseFor, RpcError> {
    let stale = |m: &str| Err(RpcError::Node { code: -1, message: format!("template-stale: {}", m) });
    if t["previousblockhash"].as_str() != Some(prev) {
        return stale("previousblockhash is not the tip");
    }
    let mut fees = 0u64;
    for x in t["transactions"].as_array().ok_or_else(|| fixture("no transactions"))? {
        if txids.iter().any(|id| x["hash"].as_str() == Some(id.as_str())) {
            fees += x["fee"].as_u64().ok_or_else(|| fixture("a transaction without a fee"))?;
        }
    }
    let coinbase_of = |v: &Value| -> Result<Vec<u8>, RpcError> { hex::decode(v["coinbasetxn"]["data"].as_str().ok_or_else(|| fixture("no coinbasetxn"))?).map_err(fixture) };
    let (data, kind) = if address.starts_with("yregtestsapling1") {
        // the helper node's coinbase: valid only for the vector's transactions
        let h: Value = serde_json::from_str(HELPER_TEMPLATE).map_err(fixture)?;
        if fees != 489 {
            return stale("transaction is not in the mempool");
        }
        (coinbase_of(&h)?, "sapling")
    } else {
        let mut cb = Coinbase::parse(&coinbase_of(t)?).map_err(fixture)?;
        cb.vout.first_mut().ok_or_else(|| fixture("a coinbase without outputs"))?.script_pubkey = pq_spk(address);
        (cb.serialize(), "pq")
    };
    let mut h = dsha256(&data);
    h.reverse();
    let height = t["height"].as_u64().and_then(|v| u32::try_from(v).ok()).ok_or_else(|| fixture("no height"))?;
    Ok(CoinbaseFor {
        data: hex::encode(&data),
        hash: hex::encode(h),
        height,
        previousblockhash: prev.to_string(),
        addresstype: kind.into(),
        subsidy: 625_000_000,
        fund: 0,
        fundrule: "none".into(),
        fees,
        amount: 625_000_000 + fees,
        txcount: txids.len(),
        flags: String::new(),
        sigops: 0,
    })
}

impl CoinbaseSource for FakeSource {
    fn coinbase_for(&self, address: &str, previousblockhash: &str, txids: &[String]) -> Result<CoinbaseFor, RpcError> {
        let n = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(n, Ordering::SeqCst);
        std::thread::sleep(self.delay);
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).push((address.to_string(), previousblockhash.to_string(), txids.to_vec()));
        let mode = *self.mode.lock().unwrap_or_else(|e| e.into_inner());
        let t = self.template.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let r = match mode {
            Mode::StaleOnce => {
                *self.mode.lock().unwrap_or_else(|e| e.into_inner()) = Mode::Good;
                Err(RpcError::Node { code: -1, message: "template-stale: previousblockhash is not the tip".into() })
            }
            Mode::WrongFees => answer(&t, address, previousblockhash, txids).map(|a| CoinbaseFor { fees: a.fees.wrapping_sub(1), ..a }),
            Mode::NoMethod => Err(RpcError::Node { code: -32601, message: "Method not found".into() }),
            Mode::DoesNotFit => Err(RpcError::Node { code: -1, message: "coinbase-does-not-fit: block size 2000377 exceeds 2000000".into() }),
            Mode::Good => answer(&t, address, previousblockhash, txids),
        };
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        r
    }

    fn has_call(&self) -> Result<bool, RpcError> {
        Ok(self.call.load(Ordering::SeqCst))
    }
}

struct Pool {
    addr: SocketAddr,
    status: SocketAddr,
    template: Arc<Mutex<Value>>,
    submitted: Arc<Mutex<Vec<String>>>,
    source: Option<Arc<FakeSource>>,
    state: yolo::state::State,
}

async fn start(text: Option<&str>, source: bool, delay: Duration, concurrency: usize) -> Pool {
    start_limits(text, source, delay, yolo::Limits { coinbase_concurrency: concurrency, ..yolo::Limits::default() }).await
}

async fn start_limits(text: Option<&str>, source: bool, delay: Duration, limits: yolo::Limits) -> Pool {
    let template = Arc::new(Mutex::new(serde_json::from_str::<Value>(POOL_TEMPLATE).unwrap()));
    let submitted = Arc::new(Mutex::new(Vec::new()));
    let node = fake_node(template.clone(), submitted.clone()).await;
    let fake = source.then(|| FakeSource::new(template.clone(), delay));
    let config = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        status_bind: Some("127.0.0.1:0".parse().unwrap()),
        payout: None,
        text: text.map(|t| t.as_bytes().to_vec()),
        no_flags: false,
        password: None,
        equihash: EquihashArg::Fixed(Equihash::REGTEST),
        // the submits below carry made-up solutions: the pool's own PoW check is tested elsewhere
        limits: yolo::Limits { check_pow: false, ..limits },
        test_coinbase_source: fake.clone().map(|f| f as Arc<dyn CoinbaseSource>),
    };
    let rpc = RpcClient::new(&format!("http://{}", node), &RpcAuth { user: "u".into(), password: "p".into() });
    let bound = yolo::bind(rpc, config).await.unwrap();
    let (addr, status, state) = (bound.addr, bound.status_addr.unwrap(), bound.state());
    tokio::spawn(bound.serve());
    for _ in 0..200 {
        if state.lock().template.is_some() {
            return Pool { addr, status, template, submitted, source: fake, state };
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no template");
}

struct Miner {
    rd: tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
    wr: tokio::net::tcp::OwnedWriteHalf,
}

impl Miner {
    async fn connect(addr: SocketAddr) -> Miner {
        let (rd, wr) = TcpStream::connect(addr).await.unwrap().into_split();
        Miner { rd: BufReader::new(rd).lines(), wr }
    }
    async fn send(&mut self, line: &str) {
        self.wr.write_all(format!("{}\n", line).as_bytes()).await.unwrap();
    }
    /// The next line within `ms`; None on timeout or close.
    async fn next(&mut self, ms: u64) -> Option<Value> {
        match tokio::time::timeout(Duration::from_millis(ms), self.rd.next_line()).await {
            Ok(Ok(Some(l))) => Some(serde_json::from_str(&l).unwrap()),
            _ => None,
        }
    }
    async fn authorize(&mut self, user: &str) -> Value {
        self.send("{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"t\",null,\"h\",\"p\"]}").await;
        self.next(5000).await.unwrap();
        self.send(&format!("{{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"{}\",\"x\"]}}", user)).await;
        loop {
            let v = self.next(5000).await.expect("an answer to authorize");
            if v["id"] == json!(2) {
                return v;
            }
        }
    }
    /// The next `mining.notify` within `ms`.
    async fn notify(&mut self, ms: u64) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now()).as_millis() as u64;
            if left == 0 {
                return None;
            }
            match self.next(left).await {
                Some(v) if v["method"] == "mining.notify" => return Some(v["params"].clone()),
                Some(_) => continue,
                None => return None,
            }
        }
    }
    /// The next job on the template's current parent (earlier ones are skipped).
    async fn job_on(&mut self, template: &Arc<Mutex<Value>>) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let want = yolo::codec::reverse_hex(template.lock().unwrap()["previousblockhash"].as_str().unwrap());
            let left = deadline.saturating_duration_since(tokio::time::Instant::now()).as_millis() as u64;
            let j = self.notify(left).await.expect("a job on the tip");
            if j[2] == json!(want) {
                return j;
            }
        }
    }
    async fn submit(&mut self, id: u64, job: &Value) -> Value {
        let sol = format!("24{}", "ab".repeat(36));
        self.send(&format!("{{\"id\":{},\"method\":\"mining.submit\",\"params\":[\"w\",{},{},\"{}\",\"{}\"]}}", id, job[0], job[5], "0".repeat(36), sol)).await;
        loop {
            let v = self.next(10_000).await.expect("an answer to submit");
            if v["id"] == json!(id) {
                return v;
            }
        }
    }
}

async fn status(addr: SocketAddr) -> Value {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"GET /status HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
    let mut text = String::new();
    s.read_to_string(&mut text).await.unwrap();
    serde_json::from_str(text.split("\r\n\r\n").nth(1).unwrap()).unwrap()
}

/// The coinbase of a submitted block (after the 140-byte header, the 37-byte solution and the
/// one-byte transaction count) and the rest of the transactions.
fn split_block(block: &str, coinbase_len: usize) -> (String, String, u8) {
    let body = &block[2 * (140 + 37)..];
    let count = u8::from_str_radix(&body[..2], 16).unwrap();
    (body[2..2 + 2 * coinbase_len].to_string(), body[2 + 2 * coinbase_len..].to_string(), count)
}

/// Three kinds on one pool: the transparent miner on the swap path, the Sapling and the
/// post-quantum miners on paired jobs; each block carries both transactions and the coinbase
/// that pays its miner.
#[tokio::test]
async fn three_kinds_on_one_pool() {
    let pool = start(None, true, Duration::ZERO, 2).await;
    let t: Value = serde_json::from_str(POOL_TEMPLATE).unwrap();
    let txs: String = t["transactions"].as_array().unwrap().iter().map(|x| x["data"].as_str().unwrap()).collect();
    let helper: Value = serde_json::from_str(HELPER_TEMPLATE).unwrap();
    let sapling_cb = helper["coinbasetxn"]["data"].as_str().unwrap().to_string();

    let mut tm = Miner::connect(pool.addr).await;
    assert_eq!(tm.authorize(T_ADDR).await["result"], json!(true));
    let mut zm = Miner::connect(pool.addr).await;
    assert_eq!(zm.authorize(&format!("{}.rig-1", Z_ADDR)).await["result"], json!(true), "a 91-character Sapling username with a worker label");
    let mut pm = Miner::connect(pool.addr).await;
    assert_eq!(pm.authorize(PQ_ADDR).await["result"], json!(true), "a 53-character post-quantum username");

    // one block each, in turn; every block moves the chain on, and each miner's next job is on the new tip
    for (m, id) in [(&mut tm, 10), (&mut zm, 11), (&mut pm, 12)] {
        let j = m.job_on(&pool.template).await;
        assert_eq!(m.submit(id, &j).await["result"], json!(true));
    }
    let blocks = pool.submitted.lock().unwrap().clone();
    assert_eq!(blocks.len(), 3);
    // transparent: the node's coinbase with the miner's script at vout[0], the whole amount
    let stock = Coinbase::parse(&hex::decode(t["coinbasetxn"]["data"].as_str().unwrap()).unwrap()).unwrap();
    let (cb, rest, count) = split_block(&blocks[0], stock.serialize().len());
    let cb = Coinbase::parse(&hex::decode(&cb).unwrap()).unwrap();
    assert_eq!((count, rest.as_str()), (3, txs.as_str()));
    assert_eq!(cb.vout.len(), 1, "no fund output at -ydf=0");
    assert_eq!(hex::encode(&cb.vout[0].script_pubkey), T_SPK);
    assert_eq!(cb.vout[0].value, 625_000_489, "subsidy plus every fee");
    // Sapling: exactly the node-built coinbase, untouched, with every transaction
    let (cb, rest, count) = split_block(&blocks[1], sapling_cb.len() / 2);
    assert_eq!(cb, sapling_cb);
    assert_eq!((count, rest.as_str()), (3, txs.as_str()));
    assert_eq!(Coinbase::parse(&hex::decode(&cb).unwrap()).unwrap().value_balance, -625_000_489);
    // post-quantum: paired, paying its script
    let (cb, rest, _) = split_block(&blocks[2], stock.serialize().len() + 10);
    let cb = Coinbase::parse(&hex::decode(&cb).unwrap()).unwrap();
    assert_eq!(rest, txs);
    assert_eq!(cb.vout[0].script_pubkey, pq_spk(PQ_ADDR));
    assert_eq!(cb.vout[0].value, 625_000_489);

    let calls = pool.source.as_ref().unwrap().calls();
    assert!(calls.iter().all(|(a, _, ids)| (a == Z_ADDR || a == PQ_ADDR) && ids.len() == 2), "only paired miners ask, with the bare address: {:?}", calls);
    let st = status(pool.status).await;
    assert_eq!(st["minersByKind"], json!({ "p2pkh": 1, "p2sh": 0, "pq": 1, "sapling": 1, "fixed": 0 }), "{}", st);
    let z = st["minerList"].as_array().unwrap().iter().find(|m| m["kind"] == "sapling").unwrap().clone();
    assert_eq!((z["address"].clone(), z["label"].clone()), (json!(Z_ADDR), json!("rig-1")));
    assert_eq!(st["coinbaseCall"], json!(true));
}

/// `--text`: the paired coinbase's scriptSig is rebuilt after the height push; its outputs and
/// shielded part are the node's, byte for byte.
#[tokio::test]
async fn text_rewrites_only_the_scriptsig_of_a_paired_coinbase() {
    let pool = start(Some("yolo"), true, Duration::ZERO, 2).await;
    let helper: Value = serde_json::from_str(HELPER_TEMPLATE).unwrap();
    let node_cb = Coinbase::parse(&hex::decode(helper["coinbasetxn"]["data"].as_str().unwrap()).unwrap()).unwrap();
    let mut zm = Miner::connect(pool.addr).await;
    zm.authorize(Z_ADDR).await;
    let j = zm.notify(5000).await.unwrap();
    assert_eq!(zm.submit(5, &j).await["result"], json!(true));
    let block = pool.submitted.lock().unwrap()[0].clone();
    let len = node_cb.serialize().len() - node_cb.script_sig.len() + 2 + 5;
    let (cb, _, _) = split_block(&block, len);
    let cb = Coinbase::parse(&hex::decode(&cb).unwrap()).unwrap();
    assert_eq!(cb.script_sig, [&[0x01, 0x6f][..], &[4], b"yolo"].concat(), "height push 111, push(text)");
    assert_eq!((cb.vout, cb.value_balance, cb.shielded), (node_cb.vout, node_cb.value_balance, node_cb.shielded));
}

/// A refused answer (§8.3) issues no job; a `template-stale` answer is followed by a new request.
#[tokio::test]
async fn refused_and_stale_answers() {
    let pool = start(None, true, Duration::ZERO, 2).await;
    let source = pool.source.clone().unwrap();
    *source.mode.lock().unwrap() = Mode::WrongFees;
    let mut zm = Miner::connect(pool.addr).await;
    zm.authorize(Z_ADDR).await;
    assert!(zm.notify(2500).await.is_none(), "no job from an answer whose fees disagree with the template");
    let st = status(pool.status).await;
    assert!(st["pairs"]["failures"].as_u64().unwrap() >= 1, "{}", st);
    assert_eq!(st["pairs"]["waiting"], json!(1));
    // stale once, then good: the retry comes and so does the job
    *source.mode.lock().unwrap() = Mode::StaleOnce;
    assert!(zm.notify(15_000).await.is_some(), "a job after the stale answer");
    assert!(source.calls().len() >= 3);
}

/// A template change in the middle of a job: a changed transaction set renews the pair (not
/// before `TX_REFRESH`), with the new ids; a new block renews it at once and the old job is void.
#[tokio::test]
async fn template_changes_renew_the_pair() {
    let pool = start(None, true, Duration::ZERO, 2).await;
    let source = pool.source.clone().unwrap();
    let mut pm = Miner::connect(pool.addr).await;
    pm.authorize(PQ_ADDR).await;
    let first = pm.notify(5000).await.unwrap();
    // one transaction leaves the template (the stock coinbase claims its fee no more)
    {
        let mut t = pool.template.lock().unwrap();
        let fee = t["transactions"][1]["fee"].as_u64().unwrap();
        t["transactions"].as_array_mut().unwrap().pop();
        let mut cb = Coinbase::parse(&hex::decode(t["coinbasetxn"]["data"].as_str().unwrap()).unwrap()).unwrap();
        cb.vout[0].value -= fee;
        t["coinbasetxn"]["data"] = json!(hex::encode(cb.serialize()));
    }
    let started = tokio::time::Instant::now();
    let second = pm.notify(10_000).await.expect("a renewed pair");
    assert!(started.elapsed() >= Duration::from_secs(3), "a changed transaction set waits for TX_REFRESH");
    assert_ne!(second[3], first[3], "another merkle root");
    assert_eq!(source.calls().last().unwrap().2.len(), 1, "the new reference names one transaction");
    // a new block: the old job's submit is stale, the new pair comes at once
    let new_prev = format!("{:064x}", 7);
    pool.template.lock().unwrap()["previousblockhash"] = json!(new_prev);
    let started = tokio::time::Instant::now();
    let third = pm.notify(4000).await.expect("a pair for the new tip");
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_eq!(third[2], json!(yolo::codec::reverse_hex(&new_prev)));
    let v = pm.submit(30, &second).await;
    assert_eq!(v["error"][0], json!(21), "a job on the old parent is stale: {}", v);
    assert_eq!(source.calls().last().unwrap().1, new_prev);
}

/// At most `--coinbase-concurrency` requests in flight, whatever the number of paired miners.
#[tokio::test]
async fn requests_in_flight_are_capped() {
    let pool = start(None, true, Duration::from_millis(300), 2).await;
    let mut miners = Vec::new();
    for i in 0..5 {
        let mut m = Miner::connect(pool.addr).await;
        let a = format!("rqf{:050}", i);
        assert_eq!(m.authorize(&a).await["result"], json!(true));
        miners.push(m);
    }
    for m in miners.iter_mut() {
        assert!(m.notify(10_000).await.is_some());
    }
    let source = pool.source.unwrap();
    assert_eq!(source.max_in_flight.load(Ordering::SeqCst), 2, "the cap holds and is used");
    assert!(source.calls().len() >= 5);
}

/// Usernames that are refused: a bad worker label, an address of no payable kind, and a paired
/// kind on a node without `getcoinbasetemplatefor`.
#[tokio::test]
async fn refused_usernames() {
    let pool = start(None, true, Duration::ZERO, 2).await;
    for user in [format!("{}.", T_ADDR), format!("{}.a b", T_ADDR), format!("{}.{}", T_ADDR, "x".repeat(33)), "nonsense".to_string()] {
        let mut m = Miner::connect(pool.addr).await;
        assert_eq!(m.authorize(&user).await["error"], json!("Invalid address"), "{}", user);
    }
    let mut m = Miner::connect(pool.addr).await;
    assert_eq!(m.authorize(&format!("{}.rig_1", T_ADDR)).await["result"], json!(true));
    // no test source: the node has no getcoinbasetemplatefor, so only transparent usernames
    let plain = start(None, false, Duration::ZERO, 2).await;
    for (user, kind) in [(Z_ADDR, "sapling"), (PQ_ADDR, "pq")] {
        let mut m = Miner::connect(plain.addr).await;
        let want = format!("Invalid address: this pool cannot pay {} addresses now (its node does not serve getcoinbasetemplatefor)", kind);
        assert_eq!(m.authorize(user).await["error"], json!(want), "{}", user);
    }
    let mut m = Miner::connect(plain.addr).await;
    assert_eq!(m.authorize(T_ADDR).await["result"], json!(true));
    assert!(m.notify(5000).await.is_some(), "the swap path works as before");
    assert_eq!(status(plain.status).await["coinbaseCall"], json!(false));
}

/// Many paired addresses over many polls of an unchanged transaction set pin one copy of each
/// transaction, not one per pair (review finding 1: a full block's hex is megabytes, and every
/// pair keeps the template it was built for).
#[tokio::test]
async fn pairs_share_one_copy_of_an_unchanged_transaction_set() {
    let limits = yolo::Limits { coinbase_concurrency: 4, max_paired_per_ip: 1000, ..yolo::Limits::default() };
    let pool = start_limits(None, true, Duration::from_millis(5), limits).await;
    let ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();
    let addresses: Vec<String> = (0..200).map(|i| format!("rqf{:050}", i)).collect();
    for a in &addresses {
        pool.state.pairs.subscribe(a, yolo::pairs::PayoutKind::Pq(pq_spk(a)), ip).unwrap();
    }
    // the poller's own polls go on; besides them, re-apply the template (a new `curtime` each
    // time, the same transactions) while the pairs are being made
    let base: Value = serde_json::from_str(POOL_TEMPLATE).unwrap();
    let mut polls = 0u64;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while addresses.iter().any(|a| pool.state.pairs.current(a).is_none()) {
        assert!(tokio::time::Instant::now() < deadline, "the pairs were not all made");
        let mut t = base.clone();
        t["curtime"] = json!(t["curtime"].as_i64().unwrap() + polls as i64 % 3);
        yolo::poller::apply_template(&pool.state, serde_json::from_value(t).unwrap());
        polls += 1;
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(polls >= 50, "many polls while the pairs were made: {}", polls);
    let pairs: Vec<_> = addresses.iter().map(|a| pool.state.pairs.current(a).unwrap().1).collect();
    let templates: HashSet<*const yolo::template::BlockTemplate> = pairs.iter().map(|p| Arc::as_ptr(&p.template)).collect();
    assert!(templates.len() > 10, "the pairs hold templates of many polls ({})", templates.len());
    for k in 0..2 {
        let copies: HashSet<*const yolo::template::TemplateTx> = pairs.iter().map(|p| Arc::as_ptr(&p.template.transactions[k])).collect();
        assert_eq!(copies.len(), 1, "transaction {}: one live copy for {} pairs over {} polls", k, pairs.len(), polls);
    }
    let current = pool.state.lock().template.clone().unwrap();
    assert!(Arc::ptr_eq(&current.transactions[0], &pairs[0].template.transactions[0]), "the pool's own template shares it too");
}

/// `--max-paired-per-ip`: over the cap, a further Sapling or post-quantum address from the same
/// IP is refused with a clear message; transparent usernames still work.
#[tokio::test]
async fn paired_addresses_per_ip_are_capped() {
    let limits = yolo::Limits { max_paired_per_ip: 1, ..yolo::Limits::default() };
    let pool = start_limits(None, true, Duration::ZERO, limits).await;
    let mut zm = Miner::connect(pool.addr).await;
    assert_eq!(zm.authorize(Z_ADDR).await["result"], json!(true));
    let mut again = Miner::connect(pool.addr).await;
    assert_eq!(again.authorize(&format!("{}.rig2", Z_ADDR)).await["result"], json!(true), "the same address again is not another one");
    let mut pm = Miner::connect(pool.addr).await;
    assert_eq!(pm.authorize(PQ_ADDR).await["error"], json!("Too many Sapling and post-quantum payout addresses from this IP (at most 1)"));
    let mut tm = Miner::connect(pool.addr).await;
    assert_eq!(tm.authorize(T_ADDR).await["result"], json!(true));
    assert!(tm.notify(5000).await.is_some(), "the swap path is not capped");
    assert!(zm.notify(5000).await.is_some());
    // the address is released with its last connection
    drop(zm);
    drop(again);
    let ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();
    for _ in 0..100 {
        if pool.state.pairs.paired_per_ip(ip) == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut pm = Miner::connect(pool.addr).await;
    assert_eq!(pm.authorize(PQ_ADDR).await["result"], json!(true));
}

/// A "Method not found" answer to a request switches paired jobs off: the paired miner is
/// disconnected, paired usernames are refused with the clear message, transparent ones still
/// work; a later probe that finds the call switches them on again (review finding 5).
#[tokio::test]
async fn method_not_found_switches_paired_jobs_off_until_a_probe_finds_the_call() {
    let limits = yolo::Limits { coinbase_probe_every: Duration::from_millis(300), ..yolo::Limits::default() };
    let pool = start_limits(None, true, Duration::ZERO, limits).await;
    let source = pool.source.clone().unwrap();
    *source.mode.lock().unwrap() = Mode::NoMethod;
    source.call.store(false, Ordering::SeqCst);
    let mut zm = Miner::connect(pool.addr).await;
    assert_eq!(zm.authorize(Z_ADDR).await["result"], json!(true), "the startup check assumed the call");
    // the first request is answered "Method not found": the miner is let go, not left without work
    let closed = async {
        loop {
            match zm.rd.next_line().await {
                Ok(Some(l)) => assert!(!l.contains("mining.notify"), "no job: {}", l),
                _ => return,
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), closed).await.expect("the paired miner is disconnected");
    assert_eq!(status(pool.status).await["coinbaseCall"], json!(false));
    let mut m = Miner::connect(pool.addr).await;
    assert_eq!(m.authorize(Z_ADDR).await["error"], json!("Invalid address: this pool cannot pay sapling addresses now (its node does not serve getcoinbasetemplatefor)"));
    let mut tm = Miner::connect(pool.addr).await;
    assert_eq!(tm.authorize(T_ADDR).await["result"], json!(true));
    assert!(tm.notify(5000).await.is_some(), "the swap path is unaffected");
    // the node is upgraded: the next probe finds the call
    *source.mode.lock().unwrap() = Mode::Good;
    source.call.store(true, Ordering::SeqCst);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while status(pool.status).await["coinbaseCall"] != json!(true) {
        assert!(tokio::time::Instant::now() < deadline, "the probe did not find the call");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut zm = Miner::connect(pool.addr).await;
    assert_eq!(zm.authorize(Z_ADDR).await["result"], json!(true));
    assert!(zm.notify(5000).await.is_some(), "paired jobs again");
}

/// A keepalive never re-sends a paired job whose parent is no longer the tip (review finding 6);
/// it does re-send a live one.
#[tokio::test]
async fn keepalive_does_not_renotify_a_void_paired_job() {
    let limits = yolo::Limits { keepalive: Duration::from_millis(400), idle_keepalives: 1000, ..yolo::Limits::default() };
    let pool = start_limits(None, true, Duration::ZERO, limits).await;
    let source = pool.source.clone().unwrap();
    let mut pm = Miner::connect(pool.addr).await;
    pm.authorize(PQ_ADDR).await;
    let first = pm.notify(5000).await.unwrap();
    let again = pm.notify(2000).await.expect("a keepalive re-sends the live job");
    assert_eq!(again[0], first[0]);
    assert_eq!(again[7], json!(false), "a keepalive is not a clean job");
    // a new block, and the new pair is refused for a while: the old job is void
    *source.mode.lock().unwrap() = Mode::WrongFees;
    let new_prev = format!("{:064x}", 9);
    pool.template.lock().unwrap()["previousblockhash"] = json!(new_prev);
    let quiet = tokio::time::Instant::now() + Duration::from_millis(2500);
    while let Some(j) = pm.notify(quiet.saturating_duration_since(tokio::time::Instant::now()).as_millis() as u64).await {
        // a re-send that raced the poller's 1-second fetch of the new tip may come once
        assert!(tokio::time::Instant::now() < quiet - Duration::from_millis(1200), "a void job was re-sent: {}", j);
    }
    *source.mode.lock().unwrap() = Mode::Good;
    let next = pm.notify(10_000).await.expect("the new pair");
    assert_eq!(next[2], json!(yolo::codec::reverse_hex(&new_prev)));
}

/// `coinbase-does-not-fit` (decision D-3): the miner keeps its previous pair; its job stays
/// valid, is not replaced, and a block found on it is submitted with that pair's coinbase and
/// transactions.
#[tokio::test]
async fn coinbase_does_not_fit_keeps_the_previous_pair() {
    let pool = start(None, true, Duration::ZERO, 2).await;
    let source = pool.source.clone().unwrap();
    let mut pm = Miner::connect(pool.addr).await;
    pm.authorize(PQ_ADDR).await;
    let first = pm.notify(5000).await.unwrap();
    // the transaction set changes, and the node refuses the new set's coinbase
    *source.mode.lock().unwrap() = Mode::DoesNotFit;
    {
        let mut t = pool.template.lock().unwrap();
        let fee = t["transactions"][1]["fee"].as_u64().unwrap();
        t["transactions"].as_array_mut().unwrap().pop();
        let mut cb = Coinbase::parse(&hex::decode(t["coinbasetxn"]["data"].as_str().unwrap()).unwrap()).unwrap();
        cb.vout[0].value -= fee;
        t["coinbasetxn"]["data"] = json!(hex::encode(cb.serialize()));
    }
    let calls = source.calls().len();
    assert!(pm.notify(8000).await.is_none(), "no new job: the miner keeps its pair");
    assert!(source.calls().len() > calls, "the new set was asked for (and refused)");
    let st = status(pool.status).await;
    assert!(st["pairs"]["failures"].as_u64().unwrap() >= 1, "{}", st);
    assert_eq!(st["pairs"]["waiting"], json!(0), "the pair is still on the tip: {}", st["pairs"]);
    assert_eq!(pm.submit(40, &first).await["result"], json!(true), "the previous pair's job is still good");
    let block = pool.submitted.lock().unwrap()[0].clone();
    let (_, _, count) = split_block(&block, 0);
    assert_eq!(count, 3, "the block carries the pair's coinbase and both of its transactions");
}

/// `--min-block-interval`: a block found before its job is one interval old is held, its miner
/// answered `true` at once; further solutions on the tip are answered `true` too and credited as
/// shares, never stale or invalid; exactly one block reaches the node, after the interval, and it
/// is one finder's own block, byte for byte.
#[tokio::test]
async fn pacing_holds_one_block_and_submits_it_after_the_interval() {
    let interval = Duration::from_millis(2500);
    let pool = start_limits(None, true, Duration::ZERO, yolo::Limits { min_block_interval: interval, ..yolo::Limits::default() }).await;
    let helper: Value = serde_json::from_str(HELPER_TEMPLATE).unwrap();
    let sapling_cb = helper["coinbasetxn"]["data"].as_str().unwrap().to_string();

    let mut tm = Miner::connect(pool.addr).await;
    assert_eq!(tm.authorize(T_ADDR).await["result"], json!(true));
    let mut zm = Miner::connect(pool.addr).await;
    assert_eq!(zm.authorize(Z_ADDR).await["result"], json!(true));
    let mut pm = Miner::connect(pool.addr).await;
    assert_eq!(pm.authorize(PQ_ADDR).await["result"], json!(true));
    let (tj, zj, pj) = (tm.job_on(&pool.template).await, zm.job_on(&pool.template).await, pm.job_on(&pool.template).await);

    let st = status(pool.status).await;
    assert_eq!((st["minBlockInterval"].clone(), st["held"].clone(), st["heldStale"].clone()), (json!(2), Value::Null, json!(0)), "{}", st);

    let first = std::time::Instant::now();
    assert_eq!(tm.submit(10, &tj).await["result"], json!(true), "held: answered at once");
    assert!(first.elapsed() < Duration::from_millis(1000), "the answer does not wait for the release");
    assert_eq!(zm.submit(11, &zj).await["result"], json!(true), "a second block-target solution: true, a share");
    assert_eq!(pm.submit(12, &pj).await["result"], json!(true));
    // the same solution again is a duplicate, as it always was
    assert_eq!(tm.submit(13, &tj).await["error"][0], json!(22));
    assert!(pool.submitted.lock().unwrap().is_empty(), "nothing reaches the node before the interval");
    let st = status(pool.status).await;
    assert_eq!(st["held"]["height"], json!(111), "{}", st);
    assert!(st["held"]["candidates"].as_u64().unwrap() >= 1 && st["nextBlockInSeconds"].as_f64().unwrap() >= 1.0, "{}", st);
    assert_eq!((st["shares"].clone(), st["paceCandidates"].clone(), st["accepted"].clone(), st["stale"].clone()), (json!(3), json!(3), json!(0), json!(0)), "{}", st);

    // every miner still holds its job and is not sent a new one because of the hold
    assert!(tm.notify(300).await.is_none(), "no restart after a held block");

    for _ in 0..100 {
        if !pool.submitted.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(first.elapsed() >= interval - Duration::from_millis(50), "released {:?} after the first solution", first.elapsed());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let blocks = pool.submitted.lock().unwrap().clone();
    assert_eq!(blocks.len(), 1, "one block for the tip");
    // the block is one finder's own: its coinbase pays that finder in full
    let t: Value = serde_json::from_str(POOL_TEMPLATE).unwrap();
    let stock = Coinbase::parse(&hex::decode(t["coinbasetxn"]["data"].as_str().unwrap()).unwrap()).unwrap();
    let body = &blocks[0][2 * (140 + 37) + 2..];
    let pays_sapling = body.starts_with(&sapling_cb);
    let pays = |len: usize, spk: &[u8]| Coinbase::parse(&hex::decode(&body[..2 * len]).unwrap()).is_ok_and(|cb| cb.vout.first().is_some_and(|o| o.script_pubkey == spk && o.value == 625_000_489));
    let pays_t = pays(stock.serialize().len(), &hex::decode(T_SPK).unwrap());
    let pays_pq = pays(stock.serialize().len() + 10, &pq_spk(PQ_ADDR));
    assert_eq!([pays_t, pays_sapling, pays_pq].iter().filter(|x| **x).count(), 1, "exactly one finder is paid, in full");
    let st = status(pool.status).await;
    // rejected: the one duplicate above, nothing else
    assert_eq!((st["accepted"].clone(), st["rejected"].clone(), st["heldStale"].clone(), st["held"].clone()), (json!(1), json!(1), json!(0), Value::Null), "{}", st);
    // the chain moved on: each miner gets work on the new tip
    tm.job_on(&pool.template).await;
    zm.job_on(&pool.template).await;
}

/// Another miner's block arrives while one is held: the held block is dropped and never
/// submitted (it would be a fork block on a stale tip), and the pool mines on the new tip.
#[tokio::test]
async fn pacing_drops_the_held_block_when_the_tip_changes() {
    let interval = Duration::from_millis(1500);
    let pool = start_limits(None, false, Duration::ZERO, yolo::Limits { min_block_interval: interval, ..yolo::Limits::default() }).await;
    let mut tm = Miner::connect(pool.addr).await;
    assert_eq!(tm.authorize(T_ADDR).await["result"], json!(true));
    let job = tm.job_on(&pool.template).await;
    assert_eq!(tm.submit(10, &job).await["result"], json!(true));
    assert_eq!(status(pool.status).await["held"]["height"], json!(111));
    // somebody else's block: the node's template moves to another parent
    other_miners_block(&pool.template);
    let next = tm.job_on(&pool.template).await;
    tokio::time::sleep(interval + Duration::from_millis(500)).await;
    assert!(pool.submitted.lock().unwrap().is_empty(), "the held block is never submitted on a stale tip");
    let st = status(pool.status).await;
    assert_eq!((st["heldStale"].clone(), st["held"].clone(), st["accepted"].clone(), st["rejected"].clone()), (json!(1), Value::Null, json!(0), json!(0)), "{}", st);
    // a late submit for the old job is stale, as without pacing; the new tip's job is held in its turn
    assert_eq!(tm.submit(11, &job).await["error"][0], json!(21));
    assert_eq!(tm.submit(12, &next).await["result"], json!(true));
    for _ in 0..100 {
        if !pool.submitted.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(pool.submitted.lock().unwrap().len(), 1);
}

/// Without the flag nothing is held and `/status` has only the zero.
#[tokio::test]
async fn pacing_off_submits_at_once() {
    let pool = start(None, false, Duration::ZERO, 2).await;
    let mut tm = Miner::connect(pool.addr).await;
    assert_eq!(tm.authorize(T_ADDR).await["result"], json!(true));
    let job = tm.job_on(&pool.template).await;
    assert_eq!(tm.submit(10, &job).await["result"], json!(true));
    assert_eq!(pool.submitted.lock().unwrap().len(), 1, "the block is at the node when the miner is answered");
    let st = status(pool.status).await;
    assert_eq!(st["minBlockInterval"], json!(0));
    for k in ["held", "heldStale", "nextBlockInSeconds", "paceCandidates"] {
        assert!(st.get(k).is_none(), "{} without --min-block-interval", k);
    }
    assert_eq!((st["accepted"].clone(), st["shares"].clone()), (json!(1), json!(0)));
}

/// Refused on mainnet at start, before anything listens; accepted there when off.
#[tokio::test]
async fn pacing_is_refused_on_mainnet() {
    let mut t = serde_json::from_str::<Value>(POOL_TEMPLATE).unwrap();
    t.as_object_mut().unwrap().insert("x-test-chain".into(), json!("main"));
    let node = fake_node(Arc::new(Mutex::new(t)), Arc::new(Mutex::new(Vec::new()))).await;
    let config = |secs: u64| Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        status_bind: None,
        payout: None,
        text: None,
        no_flags: false,
        password: None,
        equihash: EquihashArg::Fixed(Equihash::REGTEST),
        limits: yolo::Limits { min_block_interval: Duration::from_secs(secs), ..yolo::Limits::default() },
        test_coinbase_source: None,
    };
    let rpc = || RpcClient::new(&format!("http://{}", node), &RpcAuth { user: "u".into(), password: "p".into() });
    let refused = yolo::bind(rpc(), config(60)).await.err().expect("a start-up error").to_string();
    assert!(refused.contains("--min-block-interval is refused on mainnet"), "{}", refused);
    assert!(yolo::bind(rpc(), config(0)).await.is_ok(), "off: mainnet starts as before");
}

/// A job's set of remembered headers is bounded; once it is full a further solution on that job
/// cannot be told from a repeat, so under pacing it is refused `job-full` (as a share is) instead
/// of counting as one more candidate. The miner was moved to a fresh job when the set filled.
#[tokio::test]
async fn pacing_does_not_count_candidates_on_a_full_job() {
    let pool = start_limits(None, false, Duration::ZERO, yolo::Limits { min_block_interval: Duration::from_secs(30), ..yolo::Limits::default() }).await;
    let mut tm = Miner::connect(pool.addr).await;
    assert_eq!(tm.authorize(T_ADDR).await["result"], json!(true));
    let job = tm.job_on(&pool.template).await;
    let sol = format!("24{}", "ab".repeat(36));
    let mut answers = Vec::new();
    for n in 0..130u64 {
        tm.send(&format!("{{\"id\":{},\"method\":\"mining.submit\",\"params\":[\"w\",{},{},\"{:036x}\",\"{}\"]}}", 100 + n, job[0], job[5], n, sol)).await;
        loop {
            let v = tm.next(10_000).await.expect("an answer to submit");
            if v["id"] == json!(100 + n) {
                answers.push(v);
                break;
            }
        }
    }
    assert!(answers[..128].iter().all(|a| a["result"] == json!(true)), "128 solutions: each answered true");
    for a in &answers[128..] {
        assert_eq!((a["error"][0].clone(), a["error"][1].clone()), (json!(21), json!("job-full")), "{}", a);
    }
    let st = status(pool.status).await;
    assert_eq!((st["paceCandidates"].clone(), st["shares"].clone(), st["held"]["height"].clone()), (json!(128), json!(128), json!(111)), "{}", st);
    assert!(pool.submitted.lock().unwrap().is_empty());
}

/// Somebody else's block: the fake node's template moves to another parent and header root.
fn other_miners_block(template: &Arc<Mutex<Value>>) {
    let mut t = template.lock().unwrap();
    let o = t.as_object_mut().unwrap();
    o.insert("previousblockhash".into(), json!(format!("{:062x}cc", 7)));
    for k in ["lightclientroothash", "finalsaplingroothash", "blockcommitmentshash"] {
        if o.contains_key(k) {
            o.insert(k.into(), json!(format!("{:062x}dd", 7)));
        }
    }
    if let Some(r) = o.get_mut("defaultroots").and_then(Value::as_object_mut) {
        r.insert("chainhistoryroot".into(), json!(format!("{:062x}dd", 7)));
    }
}

async fn wait_until(what: &str, ms: u64, mut ok: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    while !ok().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {}", what);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// One block a parent, also while the held block's `submitblock` is in flight: a solution on a job
/// that is itself one interval old must not go to the node beside it (a slow node makes the
/// window wide). It is answered `true` at once, as a share.
#[tokio::test]
async fn pacing_submits_one_block_a_parent_while_the_release_is_in_flight() {
    let interval = Duration::from_millis(1500);
    let pool = start_limits(None, false, Duration::ZERO, yolo::Limits { min_block_interval: interval, ..yolo::Limits::default() }).await;
    pool.template.lock().unwrap().as_object_mut().unwrap().insert("x-test-submit-delay-ms".into(), json!(1500));
    let mut a = Miner::connect(pool.addr).await;
    assert_eq!(a.authorize(T_ADDR).await["result"], json!(true));
    let mut b = Miner::connect(pool.addr).await;
    assert_eq!(b.authorize(T_ADDR).await["result"], json!(true));
    let (ja, jb) = (a.job_on(&pool.template).await, b.job_on(&pool.template).await);
    assert_eq!(a.submit(10, &ja).await["result"], json!(true));
    // the release has begun: nothing is held any more, and the node has not answered yet
    wait_until("the release", 5000, async || status(pool.status).await["held"].is_null()).await;
    assert!(pool.submitted.lock().unwrap().is_empty(), "the node is still at work on the held block");
    // b's job is as old as a's: without the guard its block would go to the node now
    let asked = std::time::Instant::now();
    assert_eq!(b.submit(11, &jb).await["result"], json!(true));
    assert!(asked.elapsed() < Duration::from_millis(700), "answered as a share, not after a submitblock of its own: {:?}", asked.elapsed());
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert_eq!(pool.submitted.lock().unwrap().len(), 1, "exactly one submitblock for the parent");
    let st = status(pool.status).await;
    assert_eq!((st["accepted"].clone(), st["rejected"].clone(), st["shares"].clone(), st["heldStale"].clone()), (json!(1), json!(0), json!(2), json!(0)), "{}", st);
}

/// The tip moves while the release waits for a submit permit (a flood of submits): the check is
/// made again once the permit is there, and the block is dropped, not sent.
#[tokio::test]
async fn pacing_checks_the_tip_again_after_the_wait_for_a_permit() {
    let limits = yolo::Limits { min_block_interval: Duration::from_millis(1000), submits_in_flight: 1, ..yolo::Limits::default() };
    let pool = start_limits(None, false, Duration::ZERO, limits).await;
    let mut tm = Miner::connect(pool.addr).await;
    assert_eq!(tm.authorize(T_ADDR).await["result"], json!(true));
    let job = tm.job_on(&pool.template).await;
    assert_eq!(tm.submit(10, &job).await["result"], json!(true));
    let permit = pool.state.submit_permits.acquire().await.unwrap();
    // the release task has taken the block and waits for the permit
    wait_until("the release", 5000, async || status(pool.status).await["held"].is_null()).await;
    other_miners_block(&pool.template);
    tm.job_on(&pool.template).await;
    drop(permit);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(pool.submitted.lock().unwrap().is_empty(), "not submitted on the tip the chain left");
    let st = status(pool.status).await;
    assert_eq!((st["heldStale"].clone(), st["accepted"].clone(), st["rejected"].clone()), (json!(1), json!(0), json!(0)), "{}", st);
}

/// The tip moves while a solution waits its turn to be verified (the Equihash check runs under
/// the same permit): it is answered stale, as without pacing, and nothing is held for the old tip.
#[tokio::test]
async fn pacing_does_not_hold_a_block_whose_tip_moved_during_the_check() {
    let limits = yolo::Limits { min_block_interval: Duration::from_secs(30), submits_in_flight: 1, ..yolo::Limits::default() };
    let pool = start_limits(None, false, Duration::ZERO, limits).await;
    let mut tm = Miner::connect(pool.addr).await;
    assert_eq!(tm.authorize(T_ADDR).await["result"], json!(true));
    let job = tm.job_on(&pool.template).await;
    let permit = pool.state.submit_permits.acquire().await.unwrap();
    let sol = format!("24{}", "ab".repeat(36));
    tm.send(&format!("{{\"id\":10,\"method\":\"mining.submit\",\"params\":[\"w\",{},{},\"{}\",\"{}\"]}}", job[0], job[5], "0".repeat(36), sol)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    other_miners_block(&pool.template);
    wait_until("the pool on the new tip", 5000, async || {
        let want = pool.template.lock().unwrap()["previousblockhash"].clone();
        pool.state.lock().template.as_ref().map(|t| json!(t.previousblockhash)) == Some(want)
    })
    .await;
    drop(permit);
    let answer = loop {
        let v = tm.next(5000).await.expect("an answer to the submit");
        if v["id"] == json!(10) {
            break v;
        }
    };
    assert_eq!(answer["error"][0], json!(21), "stale: {}", answer);
    let st = status(pool.status).await;
    assert_eq!((st["held"].clone(), st["heldStale"].clone(), st["stale"].clone(), st["shares"].clone()), (Value::Null, json!(0), json!(1), json!(0)), "{}", st);
    assert!(pool.submitted.lock().unwrap().is_empty());
}

/// `submitblock` does not reach the node at the release: the block is kept and tried again a
/// second later, and is accepted then.
#[tokio::test]
async fn pacing_tries_a_failed_release_again() {
    let pool = start_limits(None, false, Duration::ZERO, yolo::Limits { min_block_interval: Duration::from_millis(1000), ..yolo::Limits::default() }).await;
    pool.template.lock().unwrap().as_object_mut().unwrap().insert("x-test-submit-fail-once".into(), json!(true));
    let mut tm = Miner::connect(pool.addr).await;
    assert_eq!(tm.authorize(T_ADDR).await["result"], json!(true));
    let job = tm.job_on(&pool.template).await;
    assert_eq!(tm.submit(10, &job).await["result"], json!(true));
    // the first attempt gets no answer; the block is held again
    wait_until("the failed attempt", 8000, async || pool.template.lock().unwrap().get("x-test-submit-fail-once").is_none()).await;
    assert!(pool.submitted.lock().unwrap().is_empty());
    wait_until("the second attempt", 8000, async || pool.submitted.lock().unwrap().len() == 1).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let st = status(pool.status).await;
    assert_eq!((st["accepted"].clone(), st["rejected"].clone(), st["heldStale"].clone(), st["held"].clone()), (json!(1), json!(0), json!(0), Value::Null), "{}", st);
    assert_eq!(pool.submitted.lock().unwrap().len(), 1);
}

/// The node goes away and comes back on mainnet behind the same URL: the pool stops with a clear
/// error before any work is built from the new chain, rather than pace there or quietly stop pacing.
#[tokio::test]
async fn pacing_stops_the_pool_when_the_node_comes_back_on_mainnet() {
    let template = Arc::new(Mutex::new(serde_json::from_str::<Value>(POOL_TEMPLATE).unwrap()));
    let node = fake_node(template.clone(), Arc::new(Mutex::new(Vec::new()))).await;
    let config = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        status_bind: None,
        payout: None,
        text: None,
        no_flags: false,
        password: None,
        equihash: EquihashArg::Fixed(Equihash::REGTEST),
        limits: yolo::Limits { min_block_interval: Duration::from_secs(60), check_pow: false, ..yolo::Limits::default() },
        test_coinbase_source: None,
    };
    let rpc = RpcClient::new(&format!("http://{}", node), &RpcAuth { user: "u".into(), password: "p".into() });
    let bound = yolo::bind(rpc, config).await.unwrap();
    let state = bound.state();
    let server = tokio::spawn(bound.serve());
    wait_until("the first template", 5000, async || state.lock().template.is_some()).await;
    let generation = state.lock().generation;
    template.lock().unwrap().as_object_mut().unwrap().insert("x-test-down".into(), json!(true));
    wait_until("the node seen down", 5000, async || !state.lock().node_up).await;
    {
        let mut t = template.lock().unwrap();
        let o = t.as_object_mut().unwrap();
        o.insert("x-test-chain".into(), json!("main"));
        o.remove("x-test-down");
    }
    let stopped = tokio::time::timeout(Duration::from_secs(15), server).await.expect("the pool stops").unwrap();
    let why = stopped.expect_err("an error, not a quiet end");
    assert!(why.contains("\"main\"") && why.contains("--min-block-interval is refused on mainnet"), "{}", why);
    let g = state.lock();
    assert!(!g.node_up && g.generation == generation, "no work was built from the mainnet template");
}
