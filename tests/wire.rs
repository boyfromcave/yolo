// Copyright (c) 2026 The Ycash developers
// Distributed under the MIT software license, see the accompanying
// file LICENSE or https://www.opensource.org/licenses/mit-license.php .

//! Wire-format parity with the Perl (plan §3.2.2, §3.2.5): the exchanges Y2 recorded against
//! `ref/yolo/stratumsolo` and `cenote` (`tests/fixtures/stratum-perl-*.jsonl`, rows
//! `{"ts","dir":"send|recv","line"}` from the miner's side) are replayed against the Rust
//! server backed by a fake node, and every line the server writes must equal the recorded
//! one byte for byte — after the three values that are not the server's to choose are
//! substituted: `nonce1` (random), the merkle root (the fixture's coinbase was not recorded)
//! and the job time (the clock).
//!
//! Ordering: the Perl interleaves notifications with responses by timing (its select loop
//! sends the target and the job in the write phase after a request was answered), so the
//! responses and the notifications are compared as two ordered streams.
//!
//! Modes became the payout × text flag pair (Y7): the `stratumsolo` fixture replays under
//! `--payout <the fixture's address>`, the `cenote` fixtures under `--text`. One
//! `stratumsolo`-only behaviour was dropped with the modes: it re-issued the target and the
//! same job after `mining.extranonce.subscribe` (`stratumsolo:101-105`), which the pool
//! variants only acknowledge; the fixture's second copy of that pair is not expected.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use yolo::equihash::{Equihash, EquihashArg};
use yolo::rpc::{RpcAuth, RpcClient};
use yolo::Config;

const TEMPLATE: &str = include_str!("vectors/regtest-template-105.json");
/// The address every fixture authorizes with (the fake node validates `sm…` of 35 chars).
const FIXTURE_ADDRESS: &str = "smJS1rf66HdSykf6spi4kbRCA2mcw3xYftW";

/// The flag pair a fixture replays under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Flags {
    /// `--payout <FIXTURE_ADDRESS>` (the `stratumsolo` policy) or unset (the username is paid).
    payout: bool,
    /// `--text www.FreeSoloMining.com` (the `cenote` policy) or unset.
    text: bool,
}

impl Flags {
    const SOLO: Flags = Flags { payout: true, text: false };
    const POOL: Flags = Flags { payout: false, text: false };
    const CENOTE: Flags = Flags { payout: false, text: true };
}

impl std::fmt::Display for Flags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "payout={} text={}", if self.payout { "fixed" } else { "username" }, self.text)
    }
}

fn config(flags: Flags, status: bool) -> Config {
    Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        status_bind: status.then(|| "127.0.0.1:0".parse().unwrap()),
        payout: flags.payout.then(|| FIXTURE_ADDRESS.to_string()),
        text: flags.text.then(|| b"www.FreeSoloMining.com".to_vec()),
        no_flags: false,
        password: None,
        equihash: EquihashArg::Fixed(Equihash::REGTEST),
        // The recorded submits were solved against the Perl's coinbase, not the fixture
        // template's, so their hashes cannot meet the target here; the replays are about
        // wire shapes, the pool's own PoW check has its own tests below.
        limits: yolo::Limits { check_pow: false, ..yolo::Limits::default() },
    }
}

#[derive(Debug, Clone)]
struct Row {
    dir: String,
    line: String,
}

fn load_fixture(name: &str) -> Vec<Row> {
    let path = format!("{}/tests/fixtures/{}", env!("CARGO_MANIFEST_DIR"), name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {}", path, e));
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap_or_else(|e| panic!("{}: bad row {:?}: {}", name, l, e));
            Row { dir: v["dir"].as_str().unwrap().to_string(), line: v["line"].as_str().unwrap().to_string() }
        })
        .collect()
}

/// A template whose header fields match the fixture's first `mining.notify` (so prevhash,
/// root, bits and target compare literally) with the regtest-105 coinbase and no transactions.
fn template_from_fixture(rows: &[Row]) -> Value {
    let notify = rows.iter().find(|r| r.dir == "recv" && r.line.contains("mining.notify")).map(|r| serde_json::from_str::<Value>(&r.line).unwrap()).expect("fixture has a mining.notify");
    let target = rows
        .iter()
        .find(|r| r.dir == "recv" && r.line.contains("mining.set_target"))
        .map(|r| serde_json::from_str::<Value>(&r.line).unwrap()["params"][0].as_str().unwrap().to_string())
        .expect("fixture has a mining.set_target");
    let p = &notify["params"];
    let rev = |s: &str| yolo::codec::reverse_hex(s);
    let mut t: Value = serde_json::from_str(TEMPLATE).unwrap();
    t["previousblockhash"] = json!(rev(p[2].as_str().unwrap()));
    t["lightclientroothash"] = json!(rev(p[4].as_str().unwrap()));
    t["finalsaplingroothash"] = t["lightclientroothash"].clone();
    t["bits"] = json!(rev(p[6].as_str().unwrap()));
    t["version"] = json!(u32::from_le_bytes(hex::decode(p[1].as_str().unwrap()).unwrap().try_into().unwrap()));
    t["target"] = json!(target);
    t["transactions"] = json!([]);
    t
}

/// The fake node: JSON-RPC over HTTP/1.1, four methods, records every submitblock.
async fn fake_node(template: Value) -> (SocketAddr, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let (addr, submitted, _) = fake_node_advancing(template, false).await;
    (addr, submitted)
}

/// The fake node with a chain: when `advance` is set, every accepted `submitblock` moves the
/// template one height on (new `previousblockhash`), as a real node does.
async fn fake_node_advancing(template: Value, advance: bool) -> (SocketAddr, std::sync::Arc<std::sync::Mutex<Vec<String>>>, std::sync::Arc<std::sync::Mutex<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let submitted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = submitted.clone();
    let current = std::sync::Arc::new(std::sync::Mutex::new(template));
    let chain = current.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { break };
            let chain = chain.clone();
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    let n = match socket.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(idx) = text.find("\r\n\r\n") {
                        let headers = &text[..idx];
                        let len: usize = headers.lines().find_map(|h| h.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap())).unwrap_or(0);
                        let body_start = idx + 4;
                        if buf.len() >= body_start + len {
                            let body: Value = serde_json::from_slice(&buf[body_start..body_start + len]).unwrap();
                            let method = body["method"].as_str().unwrap_or("");
                            let params = body["params"].as_array().cloned().unwrap_or_default();
                            let mut error = Value::Null;
                            let result = match method {
                                "getblockchaininfo" => json!({ "chain": "regtest", "blocks": 104 }),
                                "getblocktemplate" => chain.lock().unwrap().clone(),
                                "submitblock" => {
                                    let block = params[0].as_str().unwrap().to_string();
                                    // test-only template key: the node rejects every block with this verdict
                                    let reject = chain.lock().unwrap().get("_reject").cloned();
                                    // test-only template key: the next submitblock fails with an RPC error
                                    if let Some(e) = chain.lock().unwrap().as_object_mut().unwrap().remove("_error_once") {
                                        error = json!({ "code": -1, "message": e });
                                    }
                                    if advance {
                                        let mut t = chain.lock().unwrap();
                                        let height = t["height"].as_u64().unwrap() + 1;
                                        t["height"] = json!(height);
                                        t["previousblockhash"] = json!(format!("{:064x}", height));
                                    }
                                    seen.lock().unwrap().push(block);
                                    reject.unwrap_or(Value::Null)
                                }
                                "validateaddress" => {
                                    let a = params[0].as_str().unwrap_or("");
                                    if a.starts_with("sm") && a.len() == 35 {
                                        json!({ "isvalid": true, "address": a, "scriptPubKey": "76a914b5521b95530df65bec840c03c0e90a126c67625888ac", "ismine": false })
                                    } else {
                                        json!({ "isvalid": false })
                                    }
                                }
                                _ => json!(null),
                            };
                            let reply = json!({ "result": result, "error": error, "id": body["id"] }).to_string();
                            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", reply.len(), reply);
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.shutdown().await;
                            return;
                        }
                    }
                }
            });
        }
    });
    (addr, submitted, current)
}

async fn start_server(flags: Flags, node: SocketAddr) -> SocketAddr {
    let rpc = RpcClient::new(&format!("http://{}", node), &RpcAuth { user: "u".into(), password: "p".into() });
    let bound = yolo::bind(rpc, config(flags, false)).await.unwrap();
    let addr = bound.addr;
    let state = bound.state();
    tokio::spawn(bound.serve());
    // wait for the poller's first template
    for _ in 0..100 {
        if state.lock().template.is_some() {
            return addr;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("server never fetched a template from the fake node");
}

/// Substitutes the server's own values for the fixture's so the rest compares byte for byte.
fn normalise(actual: &str, fixture: &str) -> String {
    let a: Value = serde_json::from_str(actual).unwrap_or_else(|e| panic!("server wrote non-JSON {:?}: {}", actual, e));
    let f: Value = serde_json::from_str(fixture).unwrap();
    let mut out = actual.to_string();
    if a["method"] == "mining.notify" && f["method"] == "mining.notify" {
        for i in [3usize, 5] {
            // merkle root, ntime
            if let (Some(av), Some(fv)) = (a["params"][i].as_str(), f["params"][i].as_str()) {
                out = out.replacen(av, fv, 1);
            }
        }
    } else if let (Some(av), Some(fv)) = (a["result"][1].as_str(), f["result"][1].as_str()) {
        // mining.subscribe: nonce1
        assert_eq!(av.len(), 28, "nonce1 must be 28 hex chars like the Perl's");
        out = out.replacen(av, fv, 1);
    }
    out
}

/// Replays a fixture; returns the blocks sent to `submitblock` and the nTime of the server's
/// last job. The pool refuses an ntime other than the job's, and the job time is the clock here
/// (the template has no `curtime`), so each recorded submit is sent with the nTime of the job
/// the server handed out, as a miner echoes it.
async fn replay(name: &str, flags: Flags) -> (Vec<String>, String) {
    let rows = load_fixture(name);
    let (node, submitted) = fake_node(template_from_fixture(&rows)).await;
    let addr = start_server(flags, node).await;
    let stream = TcpStream::connect(addr).await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();

    let sends: Vec<&Row> = rows.iter().filter(|r| r.dir == "send").collect();
    let recvs: Vec<&Row> = rows.iter().filter(|r| r.dir == "recv").collect();
    let expected_responses: Vec<&Row> = recvs.iter().copied().filter(|r| !r.line.contains("\"method\"")).collect();
    // The `stratumsolo` re-issue after `mining.extranonce.subscribe` is a second, identical
    // copy of the target + job pair; the single pool acknowledges only (Y7), so a notification
    // already expected is not expected twice.
    let mut expected_notifications: Vec<&Row> = Vec::new();
    for r in recvs.iter().copied().filter(|r| r.line.contains("\"method\"")) {
        if !expected_notifications.iter().any(|e| e.line == r.line) {
            expected_notifications.push(r);
        }
    }

    // Send the client's lines in order; one submit's nonce2 is whatever the fixture holds and
    // the fake node accepts anything, as the real node did in the recording.
    let mut responses = Vec::new();
    let mut notifications: Vec<String> = Vec::new();
    let mut job_time = String::new();
    for s in &sends {
        let mut line = s.line.clone();
        let v: Value = serde_json::from_str(&line).unwrap();
        if v["method"] == "mining.submit" {
            let recorded = v["params"][2].as_str().unwrap();
            line = line.replacen(&format!("\"{}\"", recorded), &format!("\"{}\"", job_time), 1);
        }
        wr.write_all(format!("{}\n", line).as_bytes()).await.unwrap();
        // every request gets exactly one response; notifications may come before it
        loop {
            let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
                .await
                .unwrap_or_else(|_| panic!("{}: no response to {}", name, s.line))
                .unwrap()
                .unwrap_or_else(|| panic!("{}: server closed after {}", name, s.line));
            if line.contains("\"method\"") {
                let v: Value = serde_json::from_str(&line).unwrap();
                if v["method"] == "mining.notify" {
                    job_time = v["params"][5].as_str().unwrap().to_string();
                }
                notifications.push(line);
            } else {
                responses.push(line);
                break;
            }
        }
    }
    // drain the notifications that follow the last response (the re-issued job after a submit)
    while notifications.len() < expected_notifications.len() {
        match tokio::time::timeout(Duration::from_millis(500), lines.next_line()).await {
            Ok(Ok(Some(line))) if line.contains("\"method\"") => notifications.push(line),
            _ => break,
        }
    }

    assert_eq!(responses.len(), expected_responses.len(), "{}: response count", name);
    for (got, want) in responses.iter().zip(&expected_responses) {
        assert_eq!(normalise(got, &want.line), want.line, "{}: response", name);
    }
    assert_eq!(notifications.len(), expected_notifications.len(), "{}: notification count\n{:#?}", name, notifications);
    for (got, want) in notifications.iter().zip(&expected_notifications) {
        assert_eq!(normalise(got, &want.line), want.line, "{}: notification", name);
    }
    let blocks = submitted.lock().unwrap().clone();
    assert_eq!(blocks.len(), sends.iter().filter(|s| s.line.contains("mining.submit")).count(), "{}: submitblock calls", name);
    (blocks, job_time)
}

#[tokio::test]
async fn solo_matches_perl_stratumsolo() {
    let (blocks, job_time) = replay("stratum-perl-solo.jsonl", Flags::SOLO).await;
    // the block the server assembled: header fields from the fixture's notify, nonce1 ‖ nonce2,
    // the solution as sent, then the coinbase as the template gave it
    let block = &blocks[0];
    let rows = load_fixture("stratum-perl-solo.jsonl");
    let notify: Value = serde_json::from_str(&rows.iter().find(|r| r.line.contains("mining.notify")).unwrap().line).unwrap();
    let submit: Value = serde_json::from_str(&rows.iter().find(|r| r.line.contains("mining.submit")).unwrap().line).unwrap();
    let (np, sp) = (&notify["params"], &submit["params"]);
    assert_eq!(&block[..8], np[1].as_str().unwrap());
    assert_eq!(&block[8..72], np[2].as_str().unwrap());
    assert_eq!(&block[136..200], np[4].as_str().unwrap());
    assert_eq!(&block[200..208], job_time, "nTime as the miner echoed it: the job's");
    assert_eq!(&block[208..216], np[6].as_str().unwrap());
    assert_eq!(&block[244..280], sp[3].as_str().unwrap(), "nonce2 after the 28-char nonce1");
    assert_eq!(&block[280..354], sp[4].as_str().unwrap());
    assert!(block[354..].starts_with("01"));
    // `--payout` rewrites vout[0] structurally to the fixture address's script, which is the
    // template's own: the coinbase is byte for byte the node's (the `stratumsolo` outcome).
    let t: Value = serde_json::from_str(TEMPLATE).unwrap();
    assert_eq!(&block[356..], t["coinbasetxn"]["data"].as_str().unwrap());
}

#[tokio::test]
async fn cenote_matches_perl_cenote() {
    // The unpatched Perl `cenote` fixture: the wire shapes are the same; the coinbase is not
    // (the Rust server keeps the flags, which is the point: Y-F1).
    let (blocks, _) = replay("stratum-perl-cenote.jsonl", Flags::CENOTE).await;
    let cb = hex::decode(&blocks[0][356..]).unwrap();
    let cb = yolo::tx::Coinbase::parse(&cb).unwrap();
    assert_eq!(yolo::tag::tag_kind(&cb.script_sig), "quote");
    assert!(cb.script_sig.ends_with(b"www.FreeSoloMining.com"));
}

#[tokio::test]
async fn cenote_fixed_matches_perl_cenote() {
    replay("stratum-perl-cenote-fixed.jsonl", Flags::CENOTE).await;
}

#[tokio::test]
async fn every_flag_pair_uses_the_same_shapes() {
    // No Perl `stratumpool` recording exists; its wire code is the `cenote` one. The four
    // cells of the grid all speak it.
    for flags in [Flags::POOL, Flags::SOLO, Flags::CENOTE, Flags { payout: true, text: true }] {
        replay("stratum-perl-cenote.jsonl", flags).await;
    }
}

#[tokio::test]
async fn payout_unset_rejects_a_username_that_is_not_an_address() {
    let rows = load_fixture("stratum-perl-solo.jsonl");
    let (node, _) = fake_node(template_from_fixture(&rows)).await;
    let addr = start_server(Flags::POOL, node).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"t\",null,\"127.0.0.1\",\"1\"]}\n{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"worker1\",\"x\"]}\n").await.unwrap();
    let mut out = String::new();
    tokio::time::timeout(Duration::from_secs(5), s.read_to_string(&mut out)).await.expect("server should close").unwrap();
    assert!(out.ends_with("{\"id\":2,\"result\": false,\"error\": \"Invalid address\"}\n"), "{}", out);
    // with --payout the same username is a worker name
    let rows = load_fixture("stratum-perl-solo.jsonl");
    let (node, _) = fake_node(template_from_fixture(&rows)).await;
    let addr = start_server(Flags::SOLO, node).await;
    let stream = TcpStream::connect(addr).await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    wr.write_all(b"{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"t\",null,\"127.0.0.1\",\"1\"]}\n{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"worker1\",\"x\"]}\n").await.unwrap();
    let _subscribe = next_line(&mut lines).await;
    assert_eq!(next_line(&mut lines).await, "{\"id\":2,\"result\": true,\"error\": null}");
    let mut saw_job = false;
    for _ in 0..2 {
        if next_line(&mut lines).await.contains("mining.notify") {
            saw_job = true;
        }
    }
    assert!(saw_job, "a worker name gets work under --payout");
}

#[tokio::test]
async fn garbage_and_unknown_methods_disconnect() {
    let rows = load_fixture("stratum-perl-solo.jsonl");
    let (node, _) = fake_node(template_from_fixture(&rows)).await;
    let addr = start_server(Flags::SOLO, node).await;
    // garbage has no id to answer; an unknown method is answered with an error, then closed
    for (bad, reply) in [("not json\n", ""), ("{\"id\":1,\"method\":\"mining.get_transactions\",\"params\":[]}\n", "{\"id\":1,\"result\": false,\"error\": [20,\"Unknown method\",null]}\n")] {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(bad.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await.expect("server should close").unwrap();
        assert_eq!(String::from_utf8_lossy(&out), reply, "reply to {:?}", bad);
    }
}

#[tokio::test]
async fn status_endpoint_reports_the_pool() {
    let rows = load_fixture("stratum-perl-solo.jsonl");
    let (node, _) = fake_node(template_from_fixture(&rows)).await;
    let rpc = RpcClient::new(&format!("http://{}", node), &RpcAuth { user: "u".into(), password: "p".into() });
    let bound = yolo::bind(rpc, config(Flags::SOLO, true)).await.unwrap();
    let status_addr = bound.status_addr.unwrap();
    let state = bound.state();
    tokio::spawn(bound.serve());
    for _ in 0..100 {
        if state.lock().template.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut s = TcpStream::connect(status_addr).await.unwrap();
    s.write_all(b"GET /status HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "{}", out);
    let body: Value = serde_json::from_str(out.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body["height"], 105);
    assert_eq!(body["tag"], "quote");
    assert_eq!(body["payout"], FIXTURE_ADDRESS);
    assert_eq!(body["text"], false);
    assert!(body.get("mode").is_none() && body.get("cenoteLeft").is_none(), "{}", body);
    assert_eq!(body["equihash"], "48,5");
    assert_eq!(body["miners"], 0);
    let _: HashMap<String, Value> = serde_json::from_value(body).unwrap();
}

async fn next_line(lines: &mut tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>) -> String {
    tokio::time::timeout(Duration::from_secs(5), lines.next_line()).await.expect("line").unwrap().expect("open")
}

/// After an accepted `submitblock` the node has built past the template the job came from:
/// the next `mining.notify` must carry the new parent, and no job on the old parent may be
/// re-issued in between (a fast solver would re-solve it and be rejected `inconclusive`).
#[tokio::test]
async fn accepted_submit_refreshes_the_template_before_the_next_job() {
    for mode in [Flags::SOLO, Flags::POOL] {
        let rows = load_fixture("stratum-perl-solo.jsonl");
        let (node, _, _) = fake_node_advancing(template_from_fixture(&rows), true).await;
        let addr = start_server(mode, node).await;
        let stream = TcpStream::connect(addr).await.unwrap();
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        let user = if mode.payout { "stratum-miner" } else { FIXTURE_ADDRESS };
        wr.write_all(b"{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"t\",null,\"127.0.0.1\",\"1\"]}\n").await.unwrap();
        let _subscribe = next_line(&mut lines).await;
        wr.write_all(format!("{{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"{}\",\"x\"]}}\n", user).as_bytes()).await.unwrap();
        // authorize's response, then set_target and the first job
        let mut first_job = None;
        for _ in 0..3 {
            let line = next_line(&mut lines).await;
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["method"] == "mining.notify" {
                first_job = Some(v);
            }
        }
        let first_job = first_job.expect("a job after authorize");
        let old_parent = first_job["params"][2].as_str().unwrap().to_string();
        let job_id = first_job["params"][0].as_str().unwrap();
        let submit: Value = serde_json::from_str(&rows.iter().find(|r| r.line.contains("mining.submit")).unwrap().line).unwrap();
        let sp = &submit["params"];
        wr.write_all(format!("{{\"id\":4,\"method\":\"mining.submit\",\"params\":[\"{}\",\"{}\",{},{},{}]}}\n", user, job_id, first_job["params"][5], sp[3], sp[4]).as_bytes()).await.unwrap();
        let started = std::time::Instant::now();
        let mut saw_result = false;
        let mut new_job = None;
        while new_job.is_none() {
            let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
                .await
                .unwrap_or_else(|_| panic!("{}: no job on the new parent after the accepted submit", mode))
                .unwrap()
                .expect("server closed");
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == 4 {
                assert_eq!(v["result"], true, "{}: submit result {}", mode, line);
                saw_result = true;
            } else if v["method"] == "mining.notify" {
                assert!(saw_result, "{}: a job before the submit's response: {}", mode, line);
                let parent = v["params"][2].as_str().unwrap();
                assert_ne!(parent, old_parent, "{}: the old template was re-issued after the node built past it: {}", mode, line);
                new_job = Some(v);
            }
        }
        // the refresh was immediate, not the next 1 s poll
        assert!(started.elapsed() < Duration::from_millis(900), "{}: new job took {:?}", mode, started.elapsed());
        let new_job = new_job.unwrap();
        assert_eq!(new_job["params"][2], json!(yolo::codec::reverse_hex(&format!("{:064x}", 106))), "{}: parent of the new job", mode);
        assert_eq!(new_job["params"][7], json!(true), "{}: clean_jobs on the new parent", mode);
    }
}

/// A connected miner for the hardening tests: raw lines in, raw lines out.
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
    /// The next line within `secs`, or None when the server closed the socket.
    async fn next(&mut self, secs: u64) -> Option<Value> {
        match tokio::time::timeout(Duration::from_secs(secs), self.rd.next_line()).await {
            Ok(Ok(Some(l))) => Some(serde_json::from_str(&l).unwrap_or_else(|e| panic!("non-JSON {:?}: {}", l, e))),
            Ok(Ok(None)) | Ok(Err(_)) => None,
            Err(_) => panic!("no line within {} s", secs),
        }
    }
    /// The next response (non-notification) within `secs`.
    async fn response(&mut self, secs: u64) -> Option<Value> {
        loop {
            match self.next(secs).await {
                Some(v) if v.get("method").is_some() => continue,
                other => return other,
            }
        }
    }
    /// True once the server has closed the socket (within `secs`); pending lines are drained.
    async fn closed(&mut self, secs: u64) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        loop {
            match tokio::time::timeout_at(deadline, self.rd.next_line()).await {
                Ok(Ok(Some(_))) => continue,
                Ok(_) => return true,
                Err(_) => return false,
            }
        }
    }
    /// Returns the connection's nonce1.
    async fn subscribe_and_authorize(&mut self, user: &str) -> String {
        self.send("{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"t\",null,\"h\",\"p\"]}").await;
        let nonce1 = self.response(5).await.unwrap()["result"][1].as_str().expect("nonce1").to_string();
        self.send(&format!("{{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"{}\",\"x\"]}}", user)).await;
        assert_eq!(self.response(5).await.unwrap()["result"], json!(true));
        nonce1
    }
    /// Waits for the next `mining.notify`; returns its params.
    async fn notify(&mut self) -> Value {
        loop {
            let v = self.next(5).await.expect("server closed before a job");
            if v["method"] == "mining.notify" {
                return v["params"].clone();
            }
        }
    }
}

async fn start_hardened(limits: yolo::Limits, node: SocketAddr) -> (SocketAddr, SocketAddr, yolo::state::State) {
    let rpc = RpcClient::new(&format!("http://{}", node), &RpcAuth { user: "u".into(), password: "p".into() });
    let mut cfg = config(Flags::POOL, true);
    cfg.limits = limits;
    let bound = yolo::bind(rpc, cfg).await.unwrap();
    let (addr, status_addr, state) = (bound.addr, bound.status_addr.unwrap(), bound.state());
    tokio::spawn(bound.serve());
    for _ in 0..100 {
        if state.lock().template.is_some() {
            return (addr, status_addr, state);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no template");
}

async fn status(addr: SocketAddr) -> Value {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"GET /status HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
    let mut text = String::new();
    s.read_to_string(&mut text).await.unwrap();
    serde_json::from_str(text.split("\r\n\r\n").nth(1).unwrap()).unwrap()
}

/// Audit H-1: a line without a newline past the cap closes the socket; nothing is buffered.
#[tokio::test]
async fn overlong_line_disconnects() {
    let t: Value = serde_json::from_str(TEMPLATE).unwrap();
    let (node, _) = fake_node(t).await;
    let (addr, _, _) = start_hardened(yolo::Limits::default(), node).await;
    let mut m = Miner::connect(addr).await;
    m.wr.write_all(&vec![b'a'; yolo::stratum::MAX_LINE + 1]).await.unwrap();
    assert!(m.closed(5).await);
    // just under the cap and valid JSON is fine
    let mut m = Miner::connect(addr).await;
    let pad = "p".repeat(yolo::stratum::MAX_LINE - 80);
    m.send(&format!("{{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"{}\"]}}", pad)).await;
    assert!(m.response(5).await.unwrap()["result"][1].is_string());
}

/// Audit H-2: the global and per-IP caps, the authorize deadline, and the gauges.
#[tokio::test]
async fn connection_caps_and_authorize_deadline() {
    let t: Value = serde_json::from_str(TEMPLATE).unwrap();
    let (node, _) = fake_node(t).await;
    let limits = yolo::Limits { max_connections: 3, max_per_ip: 2, auth_timeout: Duration::from_millis(400), ..yolo::Limits::default() };
    let (addr, status_addr, state) = start_hardened(limits, node).await;
    // two sockets from 127.0.0.1 are the per-IP cap; the third is closed at once
    let mut a = Miner::connect(addr).await;
    let mut b = Miner::connect(addr).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut c = Miner::connect(addr).await;
    assert!(c.closed(5).await, "third socket from one IP must be refused");
    let st = status(status_addr).await;
    assert_eq!(st["connections"], json!(2));
    assert_eq!(st["miners"], json!(0), "unauthorized sockets are not miners");
    // one authorizes and counts as a miner; the other never does and is dropped at the deadline
    a.subscribe_and_authorize(FIXTURE_ADDRESS).await;
    assert_eq!(status(status_addr).await["miners"], json!(1));
    assert!(b.closed(5).await, "unauthorized socket must be closed after auth_timeout");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let st = status(status_addr).await;
    assert_eq!(st["connections"], json!(1));
    assert_eq!(st["miners"], json!(1));
    // the global cap: make it the only limit by freeing the per-IP one
    drop(a);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(state.lock().connections, 0);
    assert_eq!(state.lock().miners, 0);
    assert_eq!(state.connection_permits.available_permits(), 3);
}

/// Audit H-3, H-17: a submit above the target or for an unknown job never reaches the node,
/// and a miner that keeps sending bad submits is disconnected.
#[tokio::test]
async fn bad_submits_stay_local_and_get_the_miner_disconnected() {
    let mut t: Value = serde_json::from_str(TEMPLATE).unwrap();
    // The regtest target (0f0f0f…) admits one random hash in seventeen and the job time moves
    // the merkle root every second; a near-zero target makes the rejections deterministic.
    t["target"] = json!(format!("{}01", "0".repeat(62)));
    let (node, submitted) = fake_node(t).await;
    let limits = yolo::Limits { max_bad_submits: 3, ..yolo::Limits::default() };
    let (addr, status_addr, _) = start_hardened(limits, node).await;
    let mut m = Miner::connect(addr).await;
    m.subscribe_and_authorize(FIXTURE_ADDRESS).await;
    let mut job = m.notify().await;
    let sol = format!("24{}", "ab".repeat(36));
    // unknown job: answered stale (21), not counted as a bad submit nor as a refusal
    m.send(&format!("{{\"id\":9,\"method\":\"mining.submit\",\"params\":[\"w\",\"999\",\"b61cba6a\",\"{}\",\"{}\"]}}", "0".repeat(36), sol)).await;
    let v = m.response(5).await.unwrap();
    assert_eq!(v["result"], json!(false));
    assert_eq!(v["error"], json!([21, "Job not found", null]));
    assert_eq!(status(status_addr).await["lastSubmitVerdict"], json!("stale"));
    // three submits that hash above the regtest target: rejected locally, then disconnected
    // (a stale answer leaves the miner on its job; each refusal after it hands out a new one)
    for i in 0..3 {
        if i > 0 {
            job = m.notify().await;
        }
        m.send(&format!("{{\"id\":{},\"method\":\"mining.submit\",\"params\":[\"w\",{},{},\"{}{:02x}\",\"{}\"]}}", 10 + i, job[0], job[5], "f".repeat(34), i, sol)).await;
        let v = m.response(5).await.unwrap();
        assert_eq!(v["result"], json!(false), "submit {}", i);
        assert_eq!(v["error"], json!([23, "high-hash", null]), "submit {}", i);
    }
    assert!(m.closed(5).await, "the miner must be disconnected after max_bad_submits");
    let st = status(status_addr).await;
    assert_eq!(st["lastSubmitVerdict"], json!("high-hash"));
    assert_eq!(st["rejected"], json!(3));
    assert_eq!(st["stale"], json!(1));
    assert!(submitted.lock().unwrap().is_empty(), "nothing may reach submitblock: {:?}", submitted.lock().unwrap());
}

/// Audit H-4: one authorize per connection, a per-IP budget, and cached validateaddress.
#[tokio::test]
async fn authorize_is_once_rate_limited_and_cached() {
    let t: Value = serde_json::from_str(TEMPLATE).unwrap();
    let (node, _) = fake_node(t).await;
    let limits = yolo::Limits { authorize_per_minute: 3, ..yolo::Limits::default() };
    let (addr, _, state) = start_hardened(limits, node).await;
    let mut m = Miner::connect(addr).await;
    m.subscribe_and_authorize(FIXTURE_ADDRESS).await;
    assert!(state.cached_address(FIXTURE_ADDRESS).is_some(), "the answer is cached");
    // the same worker again is answered (no node call, no budget); another worker is refused
    // with an answer, then the socket is closed
    m.send(&format!("{{\"id\":3,\"method\":\"mining.authorize\",\"params\":[\"{}\",\"x\"]}}", FIXTURE_ADDRESS)).await;
    assert_eq!(m.response(5).await.unwrap()["result"], json!(true), "a repeated authorize for the same worker");
    m.send("{\"id\":4,\"method\":\"mining.authorize\",\"params\":[\"smOtherWorkerAddress00000000000000\",\"x\"]}").await;
    assert_eq!(m.response(5).await.unwrap()["error"], json!("Auth Failed"));
    assert!(m.closed(5).await, "an authorize for another worker closes the socket");
    // invalid answers are cached too, and the per-IP budget (3) closes the fourth attempt
    let mut m = Miner::connect(addr).await;
    m.send("{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"nope\",\"x\"]}").await;
    assert_eq!(m.response(5).await.unwrap()["result"], json!(false));
    assert_eq!(state.cached_address("nope"), Some(None));
    let mut m = Miner::connect(addr).await;
    m.send("{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"nope\",\"x\"]}").await;
    assert_eq!(m.response(5).await.unwrap()["result"], json!(false));
    let mut m = Miner::connect(addr).await;
    m.send(&format!("{{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"{}\",\"x\"]}}", FIXTURE_ADDRESS)).await;
    let v = m.response(5).await.unwrap();
    assert_eq!(v["result"], json!(false), "fourth authorize from this IP within a minute: {}", v);
    assert_eq!(v["error"], json!("Auth Failed"));
    assert!(m.closed(5).await);
}

/// Audit H-5: the password gate still works (constant-time compare), wrong password disconnects.
#[tokio::test]
async fn password_gate() {
    let t: Value = serde_json::from_str(TEMPLATE).unwrap();
    let (node, _) = fake_node(t).await;
    let rpc = RpcClient::new(&format!("http://{}", node), &RpcAuth { user: "u".into(), password: "p".into() });
    let mut cfg = config(Flags::POOL, false);
    cfg.password = Some("s3cret".into());
    let bound = yolo::bind(rpc, cfg).await.unwrap();
    let addr = bound.addr;
    tokio::spawn(bound.serve());
    let mut m = Miner::connect(addr).await;
    m.send(&format!("{{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"{}\",\"s3cre\"]}}", FIXTURE_ADDRESS)).await;
    assert_eq!(m.response(5).await.unwrap()["error"], json!("Auth Failed"));
    assert!(m.closed(5).await);
    let mut m = Miner::connect(addr).await;
    m.send(&format!("{{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"{}\",\"s3cret\"]}}", FIXTURE_ADDRESS)).await;
    assert_eq!(m.response(5).await.unwrap()["result"], json!(true));
}

/// The header a miner would hash for a job's `mining.notify` params, the connection's nonce1,
/// a nonce2 and a solution (the layout of `yolo::work::assemble_header`).
fn header_for(job: &Value, nonce1: &str, nonce2: &str, solution: &str) -> String {
    let p = |i: usize| job[i].as_str().unwrap();
    format!("{}{}{}{}{}{}{}{}{}", p(1), p(2), p(3), p(4), p(5), p(6), nonce1, nonce2, solution)
}

/// The first nonce2 from `from` on whose header the block target is met (`meets` true) or missed.
fn grind(job: &Value, nonce1: &str, solution: &str, target: &str, meets: bool, from: u64) -> String {
    (from..from + 100_000).map(|i| format!("{:036x}", i)).find(|n2| yolo::work::meets_target(&header_for(job, nonce1, n2, solution), target).unwrap() == meets).expect("a nonce2 within 100,000 tries")
}

fn submit_line(id: u64, job: &Value, ntime: &str, nonce2: &str, solution: &str) -> String {
    format!("{{\"id\":{},\"method\":\"mining.submit\",\"params\":[\"w\",{},\"{}\",\"{}\",\"{}\"]}}", id, job[0], ntime, nonce2, solution)
}

/// Nits 1-3 of the PR 11 review: with `--share-ratio` a share keeps the job and answers true, a
/// repeat is refused `duplicate`, an ntime other than the job's is refused `bad-ntime`, shares
/// never reset the bad-submit count (the share target here is the all-ones cap, where any
/// well-formed submit is a share), a miner sending only shares is never disconnected, and a
/// submit that meets the block target is still forwarded to `submitblock`.
#[tokio::test]
async fn shares_duplicates_and_ntime() {
    let t: Value = serde_json::from_str(TEMPLATE).unwrap();
    let target = t["target"].as_str().unwrap().to_string();
    let (node, submitted) = fake_node(t).await;
    // regtest's 0f0f0f… target times 32 overflows: the share target is all ones
    let limits = yolo::Limits { share_ratio: 32, max_bad_submits: 3, ..yolo::Limits::default() };
    let (addr, status_addr, state) = start_hardened(limits, node).await;
    let sol = format!("24{}", "ab".repeat(36));

    let mut m = Miner::connect(addr).await;
    let nonce1 = m.subscribe_and_authorize(FIXTURE_ADDRESS).await;
    let job = m.notify().await;
    let ntime = job[5].as_str().unwrap().to_string();
    // a miner sending only valid shares: all true, all on the one job, never disconnected
    let mut from = 0;
    let mut first_share = None;
    for id in 10..30u64 {
        let n2 = grind(&job, &nonce1, &sol, &target, false, from);
        from = u64::from_str_radix(&n2, 16).unwrap() + 1;
        first_share.get_or_insert(n2.clone());
        m.send(&submit_line(id, &job, &ntime, &n2, &sol)).await;
        let v = m.next(5).await.expect("connected");
        assert_eq!(v["id"], json!(id), "a share answers at once, with no new job before it: {}", v);
        assert_eq!(v["result"], json!(true), "share {}", id);
    }
    assert_eq!(state.lock().shares, 20);
    assert!(submitted.lock().unwrap().is_empty(), "shares never reach the node");
    // the same share again: refused, counted bad
    m.send(&submit_line(40, &job, &ntime, first_share.as_ref().unwrap(), &sol)).await;
    assert_eq!(m.response(5).await.unwrap()["result"], json!(false));
    assert_eq!(status(status_addr).await["lastSubmitVerdict"], json!("duplicate"));
    // an ntime the job did not carry: refused before hashing, counted bad
    let job = m.notify().await;
    let rolled = format!("{:08x}", u32::from_str_radix(job[5].as_str().unwrap(), 16).unwrap().wrapping_add(1));
    let n2 = grind(&job, &nonce1, &sol, &target, false, from);
    m.send(&submit_line(41, &job, &rolled, &n2, &sol)).await;
    assert_eq!(m.response(5).await.unwrap()["result"], json!(false));
    assert_eq!(status(status_addr).await["lastSubmitVerdict"], json!("bad-ntime"));
    // a good share in between does not clear the count: the third bad submit disconnects
    let job = m.notify().await;
    let ntime = job[5].as_str().unwrap().to_string();
    let n2 = grind(&job, &nonce1, &sol, &target, false, from);
    m.send(&submit_line(42, &job, &ntime, &n2, &sol)).await;
    assert_eq!(m.response(5).await.unwrap()["result"], json!(true));
    m.send(&submit_line(43, &job, &ntime, &n2, &sol)).await;
    assert_eq!(m.response(5).await.unwrap()["result"], json!(false), "duplicate");
    assert!(m.closed(5).await, "the third bad submit disconnects although a share came between");
    assert!(submitted.lock().unwrap().is_empty());

    // ratio > 1 and a submit that meets the block target with a garbage solution: the
    // Equihash check refuses it, the node never sees it
    let mut m = Miner::connect(addr).await;
    let nonce1 = m.subscribe_and_authorize(FIXTURE_ADDRESS).await;
    let job = m.notify().await;
    let ntime = job[5].as_str().unwrap().to_string();
    let n2 = grind(&job, &nonce1, &sol, &target, true, 0);
    m.send(&submit_line(50, &job, &ntime, &n2, &sol)).await;
    assert_eq!(m.response(5).await.unwrap()["result"], json!(false));
    assert_eq!(status(status_addr).await["lastSubmitVerdict"], json!("bad-solution"));
    assert!(submitted.lock().unwrap().is_empty(), "a fake solution never reaches submitblock");
}

/// A minimal Wagner solver for Equihash (48,5), the regtest parameters: the solutions (minimal
/// encoding, no compact size) of the 108-byte input and 32-byte nonce, usually one or two.
fn solve_48_5(input: &[u8], nonce: &[u8]) -> Vec<Vec<u8>> {
    const N: u32 = 48;
    const K: u32 = 5;
    const PER_HASH: u32 = 512 / N; // 10 indices per BLAKE2b output
    let mut personal = b"ZcashPoW".to_vec();
    personal.extend_from_slice(&N.to_le_bytes());
    personal.extend_from_slice(&K.to_le_bytes());
    let mut base = blake2b_simd::Params::new().hash_length((PER_HASH * N / 8) as usize).personal(&personal).to_state();
    base.update(input);
    base.update(nonce);
    let mut rows: Vec<(Vec<u8>, Vec<u32>)> = Vec::new();
    for g in 0..(1u32 << 9).div_ceil(PER_HASH) {
        let h = base.clone().update(&g.to_le_bytes()).finalize();
        for j in 0..PER_HASH {
            let i = g * PER_HASH + j;
            if i < 1 << 9 {
                let start = (j * N / 8) as usize;
                rows.push((h.as_bytes()[start..start + 6].to_vec(), vec![i]));
            }
        }
    }
    let merge = |rows: &[(Vec<u8>, Vec<u32>)], bytes: usize| -> Vec<(Vec<u8>, Vec<u32>)> {
        let mut sorted = rows.to_vec();
        sorted.sort_by(|a, b| a.0[..bytes].cmp(&b.0[..bytes]));
        let mut out = Vec::new();
        let mut s = 0;
        while s < sorted.len() {
            let mut e = s + 1;
            while e < sorted.len() && sorted[e].0[..bytes] == sorted[s].0[..bytes] {
                e += 1;
            }
            for a in s..e {
                for b in a + 1..e {
                    let (x, y) = (&sorted[a], &sorted[b]);
                    if x.1.iter().any(|i| y.1.contains(i)) {
                        continue;
                    }
                    let hash = x.0.iter().zip(&y.0).skip(1).map(|(p, q)| p ^ q).collect();
                    let (first, second) = if x.1[0] < y.1[0] { (x, y) } else { (y, x) };
                    out.push((hash, [first.1.clone(), second.1.clone()].concat()));
                }
            }
            s = e;
        }
        out
    };
    for _ in 0..K - 1 {
        rows = merge(&rows, 1);
    }
    // the last merge collides on both remaining bytes
    let mut sols = Vec::new();
    for (_, indices) in merge(&rows, 2) {
        let mut bits = Vec::with_capacity(36);
        let (mut acc, mut n): (u32, u32) = (0, 0);
        for i in indices {
            acc = (acc << 9) | i;
            n += 9;
            while n >= 8 {
                n -= 8;
                bits.push((acc >> n) as u8);
                acc &= (1 << n) - 1;
            }
        }
        sols.push(bits);
    }
    sols
}

/// The first nonce2 (and its solution, compact size included) that solves a job at (48,5).
fn solved(job: &Value, nonce1: &str, from: u64) -> (String, String) {
    let p = |i: usize| job[i].as_str().unwrap();
    let input = hex::decode(format!("{}{}{}{}{}{}", p(1), p(2), p(3), p(4), p(5), p(6))).unwrap();
    (from..)
        .find_map(|i| {
            let n2 = format!("{:036x}", i);
            let nonce = hex::decode(format!("{}{}", nonce1, n2)).unwrap();
            solve_48_5(&input, &nonce).into_iter().next().map(|s| (n2, format!("24{}", hex::encode(s))))
        })
        .unwrap()
}

/// Finding 4 of the PR 11 review: a submit that meets the block target is forwarded only with
/// a valid Equihash solution, also with `--share-ratio` > 1; a node rejection counts as a bad
/// submit.
#[tokio::test]
async fn blocks_are_verified_before_submitblock() {
    let mut t: Value = serde_json::from_str(TEMPLATE).unwrap();
    // every hash meets an all-ones block target: the Equihash check is all that is left
    t["target"] = json!("f".repeat(64));
    let (node, submitted) = fake_node(t).await;
    let limits = yolo::Limits { share_ratio: 32, ..yolo::Limits::default() };
    let (addr, status_addr, _) = start_hardened(limits, node).await;
    let mut m = Miner::connect(addr).await;
    let nonce1 = m.subscribe_and_authorize(FIXTURE_ADDRESS).await;
    let job = m.notify().await;
    let ntime = job[5].as_str().unwrap().to_string();
    // a garbage solution: refused locally
    let garbage = format!("24{}", "ab".repeat(36));
    m.send(&submit_line(60, &job, &ntime, &"0".repeat(36), &garbage)).await;
    assert_eq!(m.response(5).await.unwrap()["result"], json!(false));
    assert_eq!(status(status_addr).await["lastSubmitVerdict"], json!("bad-solution"));
    assert!(submitted.lock().unwrap().is_empty());
    // a solved one: forwarded
    let job = m.notify().await;
    let ntime = job[5].as_str().unwrap().to_string();
    let (n2, solution) = solved(&job, &nonce1, 0);
    let header = header_for(&job, &nonce1, &n2, &solution);
    assert_eq!(yolo::equihash::Equihash::REGTEST.verify_header(&hex::decode(&header).unwrap()), Ok(()), "the test solver is right");
    m.send(&submit_line(61, &job, &ntime, &n2, &solution)).await;
    assert_eq!(m.response(5).await.unwrap()["result"], json!(true));
    assert_eq!(status(status_addr).await["lastSubmitVerdict"], json!("accepted"));
    let blocks = submitted.lock().unwrap().clone();
    assert_eq!(blocks.len(), 1, "the solved block reached submitblock");
    assert!(blocks[0].starts_with(&header));
}

/// Finding 4 (b): blocks the node rejects count toward the bad-submit disconnect.
#[tokio::test]
async fn node_rejections_count_as_bad_submits() {
    let mut t: Value = serde_json::from_str(TEMPLATE).unwrap();
    t["target"] = json!("f".repeat(64));
    t["_reject"] = json!("bad-txns-test");
    let (node, submitted) = fake_node(t).await;
    let limits = yolo::Limits { max_bad_submits: 2, ..yolo::Limits::default() };
    let (addr, status_addr, _) = start_hardened(limits, node).await;
    let mut m = Miner::connect(addr).await;
    let nonce1 = m.subscribe_and_authorize(FIXTURE_ADDRESS).await;
    for id in 70..72u64 {
        let job = m.notify().await;
        // distinct nonce2 ranges: a re-issued job can carry the same header fields, and a
        // repeat would be refused `duplicate` before it reached the node
        let (n2, solution) = solved(&job, &nonce1, (id - 70) * 1_000_000);
        m.send(&submit_line(id, &job, job[5].as_str().unwrap(), &n2, &solution)).await;
        assert_eq!(m.response(5).await.unwrap()["result"], json!(false));
    }
    assert!(m.closed(5).await, "two node rejections disconnect at max_bad_submits 2");
    assert_eq!(status(status_addr).await["lastSubmitVerdict"], json!("bad-txns-test"));
    assert_eq!(submitted.lock().unwrap().len(), 2);
}

/// Re-review finding 1: the node's race verdicts (another miner won the height) are not the
/// miner's fault and never count; accepted shares slowly forgive refused submits.
#[tokio::test]
async fn race_verdicts_and_forgiven_bad_submits_keep_an_honest_miner() {
    let mut t: Value = serde_json::from_str(TEMPLATE).unwrap();
    t["target"] = json!("f".repeat(64));
    let (node, submitted, chain) = fake_node_advancing(t, false).await;
    let limits = yolo::Limits { max_bad_submits: 2, ..yolo::Limits::default() };
    let (addr, status_addr, _) = start_hardened(limits, node).await;
    let mut m = Miner::connect(addr).await;
    let nonce1 = m.subscribe_and_authorize(FIXTURE_ADDRESS).await;
    for (k, verdict) in ["inconclusive", "duplicate", "duplicate-inconclusive", "bad-prevblk"].iter().enumerate() {
        let job = m.notify().await;
        let (n2, solution) = solved(&job, &nonce1, k as u64 * 1_000_000);
        chain.lock().unwrap()["_reject"] = json!(verdict);
        m.send(&submit_line(80 + k as u64, &job, job[5].as_str().unwrap(), &n2, &solution)).await;
        assert_eq!(m.response(5).await.unwrap()["result"], json!(false));
    }
    assert_eq!(status(status_addr).await["lastSubmitVerdict"], json!("bad-prevblk"));
    assert_eq!(submitted.lock().unwrap().len(), 4);
    // still connected: a fifth request is answered
    m.send("{\"id\":90,\"method\":\"mining.extranonce.subscribe\",\"params\":[]}").await;
    assert_eq!(m.response(5).await.unwrap()["result"], json!(true));

    // decay: at max_bad_submits 2, a wrong ntime every SHARES_PER_FORGIVEN shares never
    // disconnects; two without shares between them do
    let t: Value = serde_json::from_str(TEMPLATE).unwrap();
    let target = t["target"].as_str().unwrap().to_string();
    let (node, _) = fake_node(t).await;
    let limits = yolo::Limits { share_ratio: 32, max_bad_submits: 2, ..yolo::Limits::default() };
    let (addr, _, _) = start_hardened(limits, node).await;
    let mut m = Miner::connect(addr).await;
    let nonce1 = m.subscribe_and_authorize(FIXTURE_ADDRESS).await;
    let sol = format!("24{}", "ab".repeat(36));
    let mut from = 0;
    let mut id = 100;
    let mut job = m.notify().await;
    for _ in 0..4 {
        // one refused submit (an ntime the job did not carry); the miner gets a fresh job
        m.send(&submit_line(id, &job, "00000000", &"0".repeat(36), &sol)).await;
        id += 1;
        assert_eq!(m.response(5).await.expect("still connected")["result"], json!(false));
        job = m.notify().await;
        for _ in 0..yolo::stratum::SHARES_PER_FORGIVEN {
            let n2 = grind(&job, &nonce1, &sol, &target, false, from);
            from = u64::from_str_radix(&n2, 16).unwrap() + 1;
            m.send(&submit_line(id, &job, job[5].as_str().unwrap(), &n2, &sol)).await;
            id += 1;
            assert_eq!(m.response(5).await.expect("still connected")["result"], json!(true));
        }
    }
    m.send(&submit_line(id, &job, "00000000", &"0".repeat(36), &sol)).await;
    id += 1;
    assert_eq!(m.response(5).await.unwrap()["result"], json!(false));
    job = m.notify().await;
    m.send(&submit_line(id, &job, "00000000", &"0".repeat(36), &sol)).await;
    assert!(m.closed(5).await, "refused submits without shares between them still disconnect");
}

/// Re-review finding 2: a block whose submitblock failed (RPC or task error) is not remembered,
/// so the miner's retry reaches the node instead of being refused `duplicate`.
#[tokio::test]
async fn a_block_retried_after_an_rpc_error_is_not_a_duplicate() {
    let mut t: Value = serde_json::from_str(TEMPLATE).unwrap();
    t["target"] = json!("f".repeat(64));
    t["_error_once"] = json!("node busy");
    let (node, submitted) = fake_node(t).await;
    let (addr, status_addr, _) = start_hardened(yolo::Limits::default(), node).await;
    let mut m = Miner::connect(addr).await;
    let nonce1 = m.subscribe_and_authorize(FIXTURE_ADDRESS).await;
    let job = m.notify().await;
    let (n2, solution) = solved(&job, &nonce1, 0);
    let line = submit_line(110, &job, job[5].as_str().unwrap(), &n2, &solution);
    m.send(&line).await;
    assert_eq!(m.response(5).await.unwrap()["result"], json!(false));
    assert!(status(status_addr).await["lastSubmitVerdict"].as_str().unwrap().starts_with("rpc-error"));
    // the retry on the same (still kept) job reaches the node and is accepted
    m.send(&line).await;
    assert_eq!(m.response(5).await.unwrap()["result"], json!(true));
    assert_eq!(status(status_addr).await["lastSubmitVerdict"], json!("accepted"));
    assert_eq!(submitted.lock().unwrap().len(), 2);
    // a third time: the block is on the chain, the job's parent is no longer the tip: stale,
    // never forwarded
    m.send(&line).await;
    assert_eq!(m.response(5).await.unwrap()["error"], json!([21, "Stale share", null]));
    assert_eq!(status(status_addr).await["lastSubmitVerdict"], json!("stale"));
    assert_eq!(submitted.lock().unwrap().len(), 2);
}

/// The miniZ drop on testnet-yb-fast (2026-11-13): after a new block a GPU miner flushes the
/// solutions it still holds for the previous job. Each must be answered at once with a stale
/// error (21), never forwarded as a block on a parent the chain has moved past, never counted
/// against the miner, and must not hand it yet another clean job; a job replaced at the same
/// height (the tip unchanged) is still taken.
#[tokio::test]
async fn stale_submits_are_answered_at_once_and_never_forwarded() {
    let mut t: Value = serde_json::from_str(TEMPLATE).unwrap();
    // every hash meets an all-ones block target: each solved submit is a block
    t["target"] = json!("f".repeat(64));
    let (node, submitted, _) = fake_node_advancing(t, true).await;
    let limits = yolo::Limits { max_bad_submits: 2, ..yolo::Limits::default() };
    let (addr, status_addr, _) = start_hardened(limits, node).await;
    let mut m = Miner::connect(addr).await;
    let nonce1 = m.subscribe_and_authorize(FIXTURE_ADDRESS).await;
    let old = m.notify().await;
    let ntime = old[5].as_str().unwrap().to_string();
    // three solutions for the same job, sent back to back as a GPU flushes them: the first is
    // the block, the other two are stale once it is accepted (the template still on the old
    // parent, or already the next one: stale either way)
    let (n2a, sa) = solved(&old, &nonce1, 0);
    let (n2b, sb) = solved(&old, &nonce1, 1_000_000);
    let (n2c, sc) = solved(&old, &nonce1, 2_000_000);
    m.send(&format!("{}\n{}\n{}", submit_line(1, &old, &ntime, &n2a, &sa), submit_line(2, &old, &ntime, &n2b, &sb), submit_line(3, &old, &ntime, &n2c, &sc))).await;
    let started = std::time::Instant::now();
    let a = m.response(5).await.expect("connected");
    assert_eq!((a["id"].clone(), a["result"].clone()), (json!(1), json!(true)), "{}", a);
    for id in [2, 3] {
        let v = m.response(5).await.expect("a stale submit is answered, not left hanging");
        assert_eq!(v["id"], json!(id));
        assert_eq!(v["result"], json!(false));
        assert_eq!(v["error"], json!([21, "Stale share", null]), "{}", v);
    }
    assert!(started.elapsed() < Duration::from_millis(900), "stale answers took {:?}", started.elapsed());
    assert_eq!(submitted.lock().unwrap().len(), 1, "a stale job never reaches submitblock");
    let st = status(status_addr).await;
    assert_eq!((st["accepted"].clone(), st["stale"].clone(), st["rejected"].clone()), (json!(1), json!(2), json!(0)), "{}", st);
    // exactly one clean job on the new parent: the stale answers handed out no further jobs
    let new = m.notify().await;
    assert_ne!(new[2], old[2], "the job after the block is on the new parent");
    assert_eq!(new[7], json!(true));
    m.send("{\"id\":9,\"method\":\"mining.extranonce.subscribe\",\"params\":[]}").await;
    let v = m.next(5).await.expect("connected");
    assert_eq!(v["id"], json!(9), "no other job came between: {}", v);
    // more stale submits than max_bad_submits: still connected, the new job still mines
    for id in 10..15u64 {
        let (n2, s) = solved(&old, &nonce1, 3_000_000 + id * 1_000_000);
        m.send(&submit_line(id, &old, &ntime, &n2, &s)).await;
        assert_eq!(m.response(5).await.expect("still connected")["error"][0], json!(21));
    }
    let (n2, s) = solved(&new, &nonce1, 0);
    m.send(&submit_line(20, &new, new[5].as_str().unwrap(), &n2, &s)).await;
    assert_eq!(m.response(5).await.unwrap()["result"], json!(true), "the job on the tip is a block");
    assert_eq!(submitted.lock().unwrap().len(), 2);

    // the grace: a job replaced at the same height still builds a valid block on the tip
    let mut t: Value = serde_json::from_str(TEMPLATE).unwrap();
    t["target"] = json!("f".repeat(64));
    let (node, submitted) = fake_node(t).await;
    let (addr, _, _) = start_hardened(yolo::Limits::default(), node).await;
    let mut m = Miner::connect(addr).await;
    let nonce1 = m.subscribe_and_authorize(FIXTURE_ADDRESS).await;
    let first = m.notify().await;
    // a refused submit (wrong ntime) replaces the job on the same template
    m.send(&submit_line(30, &first, "00000000", &"0".repeat(36), &format!("24{}", "ab".repeat(36)))).await;
    assert_eq!(m.response(5).await.unwrap()["error"], json!([20, "bad-ntime", null]));
    let second = m.notify().await;
    assert_ne!(second[0], first[0]);
    assert_eq!(second[2], first[2], "same parent");
    let (n2, s) = solved(&first, &nonce1, 0);
    m.send(&submit_line(31, &first, first[5].as_str().unwrap(), &n2, &s)).await;
    assert_eq!(m.response(5).await.unwrap()["result"], json!(true), "the replaced job on the tip is still taken");
    assert_eq!(submitted.lock().unwrap().len(), 1);
}

/// The authorize budget (audit H-4) charges what costs the node or fails: a miner reconnecting
/// many times a minute with an address the pool has cached is never refused, while new
/// addresses (a `validateaddress` each) and failed authorizations stay capped per IP.
#[tokio::test]
async fn reconnecting_miners_are_not_refused_and_authorize_floods_are() {
    let t: Value = serde_json::from_str(TEMPLATE).unwrap();
    let (node, _) = fake_node(t).await;
    let limits = yolo::Limits { authorize_per_minute: 3, ..yolo::Limits::default() };
    let (addr, _, _) = start_hardened(limits, node).await;
    // ten reconnects within a second (three times the budget): every one authorized
    for i in 0..10 {
        let mut m = Miner::connect(addr).await;
        m.send(&format!("{{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"{}\",\"x\"]}}", FIXTURE_ADDRESS)).await;
        assert_eq!(m.response(5).await.unwrap()["result"], json!(true), "reconnect {}", i);
    }
    // a flood of new addresses: each a node call; the budget is spent by the third (the first
    // reconnect above spent one), the next is refused and closed
    let fresh = |i: usize| format!("sm{:033}", i);
    for i in 1..3 {
        let mut m = Miner::connect(addr).await;
        m.send(&format!("{{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"{}\",\"x\"]}}", fresh(i))).await;
        assert_eq!(m.response(5).await.unwrap()["result"], json!(true), "new address {}", i);
    }
    for user in [fresh(9), "nope".to_string(), FIXTURE_ADDRESS.to_string()] {
        let mut m = Miner::connect(addr).await;
        m.send(&format!("{{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"{}\",\"x\"]}}", user)).await;
        let v = m.response(5).await.unwrap();
        assert_eq!(v["error"], json!("Auth Failed"), "{} once the budget is spent: {}", user, v);
        assert!(m.closed(5).await);
    }

    // failed authorizations are charged too (password guessing)
    let rpc = RpcClient::new(&format!("http://{}", node), &RpcAuth { user: "u".into(), password: "p".into() });
    let mut cfg = config(Flags::SOLO, false);
    cfg.password = Some("s3cret".into());
    cfg.limits = yolo::Limits { authorize_per_minute: 3, ..yolo::Limits::default() };
    let bound = yolo::bind(rpc, cfg).await.unwrap();
    let addr = bound.addr;
    tokio::spawn(bound.serve());
    // --payout and the right password: no node call, never charged
    for _ in 0..5 {
        let mut m = Miner::connect(addr).await;
        m.send("{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"rig\",\"s3cret\"]}").await;
        assert_eq!(m.response(5).await.unwrap()["result"], json!(true));
    }
    for guess in 0..4 {
        let mut m = Miner::connect(addr).await;
        m.send(&format!("{{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"rig\",\"guess{}\"]}}", guess)).await;
        assert_eq!(m.response(5).await.unwrap()["error"], json!("Auth Failed"));
        assert!(m.closed(5).await);
    }
    // the budget is spent: even the right password is refused for the rest of the minute
    let mut m = Miner::connect(addr).await;
    m.send("{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"rig\",\"s3cret\"]}").await;
    assert_eq!(m.response(5).await.unwrap()["error"], json!("Auth Failed"));
}
