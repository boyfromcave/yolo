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
    let notify = rows
        .iter()
        .find(|r| r.dir == "recv" && r.line.contains("mining.notify"))
        .map(|r| serde_json::from_str::<Value>(&r.line).unwrap())
        .expect("fixture has a mining.notify");
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
async fn fake_node_advancing(
    template: Value,
    advance: bool,
) -> (SocketAddr, std::sync::Arc<std::sync::Mutex<Vec<String>>>, std::sync::Arc<std::sync::Mutex<Value>>) {
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
                        let len: usize = headers
                            .lines()
                            .find_map(|h| h.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                            .unwrap_or(0);
                        let body_start = idx + 4;
                        if buf.len() >= body_start + len {
                            let body: Value = serde_json::from_slice(&buf[body_start..body_start + len]).unwrap();
                            let method = body["method"].as_str().unwrap_or("");
                            let params = body["params"].as_array().cloned().unwrap_or_default();
                            let result = match method {
                                "getblockchaininfo" => json!({ "chain": "regtest", "blocks": 104 }),
                                "getblocktemplate" => chain.lock().unwrap().clone(),
                                "submitblock" => {
                                    let block = params[0].as_str().unwrap().to_string();
                                    if advance {
                                        let mut t = chain.lock().unwrap();
                                        let height = t["height"].as_u64().unwrap() + 1;
                                        t["height"] = json!(height);
                                        t["previousblockhash"] = json!(format!("{:064x}", height));
                                    }
                                    seen.lock().unwrap().push(block);
                                    Value::Null
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
                            let reply = json!({ "result": result, "error": null, "id": body["id"] }).to_string();
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

async fn replay(name: &str, flags: Flags) -> Vec<String> {
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
    let mut notifications = Vec::new();
    for s in &sends {
        wr.write_all(format!("{}\n", s.line).as_bytes()).await.unwrap();
        // every request gets exactly one response; notifications may come before it
        loop {
            let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
                .await
                .unwrap_or_else(|_| panic!("{}: no response to {}", name, s.line))
                .unwrap()
                .unwrap_or_else(|| panic!("{}: server closed after {}", name, s.line));
            if line.contains("\"method\"") {
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
    blocks
}

#[tokio::test]
async fn solo_matches_perl_stratumsolo() {
    let blocks = replay("stratum-perl-solo.jsonl", Flags::SOLO).await;
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
    assert_eq!(&block[200..208], sp[2].as_str().unwrap(), "nTime as the miner echoed it");
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
    let blocks = replay("stratum-perl-cenote.jsonl", Flags::CENOTE).await;
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
    for bad in ["not json\n", "{\"id\":1,\"method\":\"mining.get_transactions\",\"params\":[]}\n"] {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(bad.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        let n = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await.expect("server should close").unwrap();
        assert_eq!(n, 0, "no reply to {:?}, got {:?}", bad, String::from_utf8_lossy(&out));
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
        wr.write_all(
            format!("{{\"id\":4,\"method\":\"mining.submit\",\"params\":[\"{}\",\"{}\",{},{},{}]}}\n", user, job_id, sp[2], sp[3], sp[4]).as_bytes(),
        )
        .await
        .unwrap();
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
