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

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use yolo::equihash::{Equihash, EquihashArg};
use yolo::rpc::{RpcAuth, RpcClient};
use yolo::work::{Mode, Policy};
use yolo::Config;

const TEMPLATE: &str = include_str!("vectors/regtest-template-105.json");

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
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let submitted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = submitted.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { break };
            let template = template.clone();
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
                                "getblocktemplate" => template.clone(),
                                "submitblock" => {
                                    seen.lock().unwrap().push(params[0].as_str().unwrap().to_string());
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
    (addr, submitted)
}

async fn start_server(mode: Mode, node: SocketAddr) -> SocketAddr {
    let rpc = RpcClient::new(&format!("http://{}", node), &RpcAuth { user: "u".into(), password: "p".into() });
    let config = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        status_bind: None,
        policy: Policy { mode, text: b"www.FreeSoloMining.com".to_vec(), scrooge: false, no_flags: false },
        password: None,
        cenote: 0,
        equihash: EquihashArg::Fixed(Equihash::REGTEST),
    };
    let bound = yolo::bind(rpc, config).await.unwrap();
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

async fn replay(name: &str, mode: Mode) -> Vec<String> {
    let rows = load_fixture(name);
    let (node, submitted) = fake_node(template_from_fixture(&rows)).await;
    let addr = start_server(mode, node).await;
    let stream = TcpStream::connect(addr).await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();

    let sends: Vec<&Row> = rows.iter().filter(|r| r.dir == "send").collect();
    let recvs: Vec<&Row> = rows.iter().filter(|r| r.dir == "recv").collect();
    let expected_responses: Vec<&Row> = recvs.iter().copied().filter(|r| !r.line.contains("\"method\"")).collect();
    let expected_notifications: Vec<&Row> = recvs.iter().copied().filter(|r| r.line.contains("\"method\"")).collect();

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
    let blocks = replay("stratum-perl-solo.jsonl", Mode::Solo).await;
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
    let t: Value = serde_json::from_str(TEMPLATE).unwrap();
    assert_eq!(&block[356..], t["coinbasetxn"]["data"].as_str().unwrap());
}

#[tokio::test]
async fn cenote_matches_perl_cenote() {
    // The unpatched Perl `cenote` fixture: the wire shapes are the same; the coinbase is not
    // (the Rust server keeps the flags, which is the point: Y-F1).
    let blocks = replay("stratum-perl-cenote.jsonl", Mode::Cenote).await;
    let cb = hex::decode(&blocks[0][356..]).unwrap();
    let cb = yolo::tx::Coinbase::parse(&cb).unwrap();
    assert_eq!(yolo::tag::tag_kind(&cb.script_sig), "quote");
    assert!(cb.script_sig.ends_with(b"www.FreeSoloMining.com"));
}

#[tokio::test]
async fn cenote_fixed_matches_perl_cenote() {
    replay("stratum-perl-cenote-fixed.jsonl", Mode::Cenote).await;
}

#[tokio::test]
async fn pool_mode_uses_the_same_shapes_as_cenote() {
    // No Perl `stratumpool` recording exists; its wire code is the `cenote` one.
    replay("stratum-perl-cenote.jsonl", Mode::Pool).await;
}

#[tokio::test]
async fn garbage_and_unknown_methods_disconnect() {
    let rows = load_fixture("stratum-perl-solo.jsonl");
    let (node, _) = fake_node(template_from_fixture(&rows)).await;
    let addr = start_server(Mode::Solo, node).await;
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
    let config = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        status_bind: Some("127.0.0.1:0".parse().unwrap()),
        policy: Policy { mode: Mode::Solo, text: vec![], scrooge: false, no_flags: false },
        password: None,
        cenote: 0,
        equihash: EquihashArg::Fixed(Equihash::REGTEST),
    };
    let bound = yolo::bind(rpc, config).await.unwrap();
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
    assert_eq!(body["mode"], "solo");
    assert_eq!(body["equihash"], "48,5");
    assert_eq!(body["miners"], 0);
    let _: HashMap<String, Value> = serde_json::from_value(body).unwrap();
}
