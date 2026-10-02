// Copyright (c) 2026 The Ycash developers
// Distributed under the MIT software license, see the accompanying
// file LICENSE or https://www.opensource.org/licenses/mit-license.php .

//! The stratum server: one task per miner, messages byte for byte as the Perl writes them
//! (`ref/yolo/stratumsolo`, `stratumpool`; plan §3.2.2). Nothing here knows about Yellowback
//! beyond logging the tag of the job it hands out.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Duration;

use serde_json::Value;
use subtle::ConstantTimeEq;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::poller::job_time;
use crate::state::State;
use crate::work::{assemble_block, assemble_header, build_work, meets_target, BuildParams, Submit, Work};

/// Bytes of nonce the pool fixes per client: the Perl's `nonce1_size` is 16 but
/// `sprintf("%04x", id) . newkey(16 - 4)` yields 4 + 24 = 28 hex chars, i.e. 14 bytes
/// (`ref/yolo/stratumsolo:77`, `newkey` at :303 doubles its argument), leaving 18 bytes of
/// nonce2 for the miner. Reproduced exactly: the recorded Perl exchanges show 28-char nonce1
/// and 36-char nonce2 (Y-F candidate: the Perl comment says "leaves 8 bytes").
pub const NONCE1_SIZE: usize = 14;
pub const NONCE1_HEX_LEN: usize = NONCE1_SIZE * 2;
pub const MINER_TIMEOUT: Duration = Duration::from_secs(60);
const JOBS_KEPT: usize = 8;

pub fn make_nonce1(client_index: u64) -> String {
    let mut random = [0u8; NONCE1_SIZE - 2];
    if getrandom::fill(&mut random).is_err() {
        // Fallback: never happens on a supported platform, but a stratum server must not die.
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        random[..8].copy_from_slice(&(t as u64).to_le_bytes());
    }
    format!("{:04x}{}", client_index & 0xffff, hex::encode(random))
}

/// The JSON id as the Perl interpolates it (`$req->{'id'}`): numbers bare, strings quoted.
fn id_text(id: &Value) -> String {
    match id {
        Value::Null => "null".into(),
        other => other.to_string(),
    }
}

pub fn msg_subscribe(id: &Value, nonce1: &str) -> String {
    format!("{{\"id\":{},\"result\":[null,\"{}\"],\"error\":null}}\n", id_text(id), nonce1)
}
pub fn msg_authorized(id: &Value) -> String {
    format!("{{\"id\":{},\"result\": true,\"error\": null}}\n", id_text(id))
}
pub fn msg_auth_failed(id: &Value, why: &str) -> String {
    format!("{{\"id\":{},\"result\": false,\"error\": \"{}\"}}\n", id_text(id), why)
}
pub fn msg_extranonce(id: &Value) -> String {
    format!("{{\"id\":{},\"result\": true,\"error\": null}}\n", id_text(id))
}
pub fn msg_set_target(target: &str) -> String {
    format!("{{\"id\":null,\"method\":\"mining.set_target\",\"params\":[\"{}\"]}}\n", target)
}
pub fn msg_notify(w: &Work, clean: bool) -> String {
    format!(
        "{{\"id\":null,\"method\":\"mining.notify\",\"params\":[\"{}\",\"{}\",\"{}\",\"{}\",\"{}\",\"{}\",\"{}\",{},\"ZcashPoW\"]}}\n",
        w.job_id, w.version, w.previousblockhash, w.merkleroot, w.light_client_root, w.time, w.bits, clean
    )
}
pub fn msg_submit_result(id: &Value, ok: bool) -> String {
    format!("{{\"id\":{},\"result\": {}}}\n", id_text(id), ok)
}

/// Audit H-1: the longest line a miner may send. A `mining.submit` at 192,7 is about 1 KB
/// (806 hex chars of solution); anything near this is not a stratum message.
pub const MAX_LINE: usize = 8192;
/// Audit H-8: miner-controlled strings are logged `{:?}` and no longer than this.
const MAX_LOGGED: usize = 128;

/// A miner-controlled string as it may appear in the log: `{:?}` (control characters and
/// escapes shown escaped), cut to `MAX_LOGGED` bytes on a character boundary.
pub fn shown(s: &str) -> String {
    let mut end = s.len().min(MAX_LOGGED);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    if end < s.len() {
        format!("{:?}…", &s[..end])
    } else {
        format!("{:?}", s)
    }
}

/// `read_line` with a bound: `Ok(Some(line))` without its terminator, `Ok(None)` at EOF, and
/// an `InvalidData` error once `MAX_LINE` bytes arrive without a newline (the caller
/// disconnects; nothing more is buffered).
pub async fn read_line_capped<R: AsyncBufRead + Unpin>(reader: &mut R, buf: &mut Vec<u8>) -> std::io::Result<Option<String>> {
    buf.clear();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if buf.is_empty() { Ok(None) } else { Ok(Some(String::from_utf8_lossy(buf).into_owned())) };
        }
        let (take, done) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (available.len(), false),
        };
        if buf.len() + take > MAX_LINE {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("line longer than {} bytes", MAX_LINE)));
        }
        buf.extend_from_slice(&available[..take]);
        reader.consume(take);
        if done {
            buf.pop();
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return Ok(Some(String::from_utf8_lossy(buf).into_owned()));
        }
    }
}

pub async fn serve(state: State, listener: TcpListener, generation_rx: watch::Receiver<u64>) {
    loop {
        match listener.accept().await {
            Ok((socket, peer)) => {
                // Audit H-2: a global cap and a per-IP cap on open sockets; a refused socket
                // is closed at once and never costs a task.
                let Ok(permit) = state.connection_permits.clone().try_acquire_owned() else {
                    warn!("miner from {} refused: {} connections open (--max-connections)", peer, state.limits.max_connections);
                    continue;
                };
                if !state.ip_connect(peer.ip()) {
                    warn!("miner from {} refused: {} connections from that address (--max-per-ip)", peer, state.limits.max_per_ip);
                    continue;
                }
                let index = {
                    let mut g = state.lock();
                    let i = g.clients_seen;
                    g.clients_seen += 1;
                    g.connections += 1;
                    i
                };
                info!("miner {} connected from {}", index, peer);
                let st = state.clone();
                let rx = generation_rx.clone();
                tokio::spawn(async move {
                    let client = Client::new(index, peer, rx);
                    let authorized = client.run(socket, st.clone()).await;
                    {
                        let mut g = st.lock();
                        g.connections -= 1;
                        if authorized {
                            g.miners -= 1;
                        }
                    }
                    st.ip_disconnect(peer.ip());
                    drop(permit);
                    info!("miner {} disconnected", index);
                });
            }
            Err(e) => {
                warn!("accept: {}", e);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

struct Client {
    index: u64,
    peer: SocketAddr,
    nonce1: String,
    generation_rx: watch::Receiver<u64>,
    auth: bool,
    ready: bool,
    mining: bool,
    software: String,
    worker: String,
    script_pubkey: Option<Vec<u8>>,
    work_number: u64,
    jobs: VecDeque<Work>,
    last_write: Instant,
    /// When the socket was accepted (the `mining.authorize` deadline counts from here).
    connected_at: Instant,
    /// When the miner last sent a line (the idle deadline counts from here).
    last_read: Instant,
    /// Submits rejected locally since the last good one (audit H-3).
    bad_submits: u32,
}

enum Action {
    Continue,
    Disconnect,
}

impl Client {
    fn new(index: u64, peer: SocketAddr, generation_rx: watch::Receiver<u64>) -> Client {
        let now = Instant::now();
        Client {
            index,
            peer,
            nonce1: make_nonce1(index),
            generation_rx,
            auth: false,
            ready: false,
            mining: false,
            software: String::new(),
            worker: String::new(),
            script_pubkey: None,
            work_number: 0,
            jobs: VecDeque::new(),
            last_write: now,
            connected_at: now,
            last_read: now,
            bad_submits: 0,
        }
    }

    /// The moment this socket is closed for silence: an unauthorized one must authorize
    /// within `auth_timeout`; an authorized one must say something within
    /// `idle_keepalives` keepalive periods (audit H-2).
    fn silence_deadline(&self, state: &State) -> Instant {
        if self.auth {
            self.last_read + MINER_TIMEOUT * state.limits.idle_keepalives
        } else {
            self.connected_at + state.limits.auth_timeout
        }
    }

    /// Serves the socket until it closes; returns whether the miner had authorized.
    async fn run(mut self, socket: TcpStream, state: State) -> bool {
        let (rd, mut wr) = socket.into_split();
        let mut reader = BufReader::new(rd);
        let mut line_buf = Vec::with_capacity(1024);
        let mut out = String::new();
        loop {
            let keepalive = tokio::time::sleep_until(self.last_write + MINER_TIMEOUT);
            let silence = tokio::time::sleep_until(self.silence_deadline(&state));
            let action = tokio::select! {
                line = read_line_capped(&mut reader, &mut line_buf) => match line {
                    Ok(Some(line)) => {
                        self.last_read = Instant::now();
                        self.handle_line(&line, &state, &mut out).await
                    }
                    Ok(None) => Action::Disconnect,
                    Err(e) => { warn!("miner {}: read: {}, disconnecting", self.index, e); Action::Disconnect }
                },
                changed = self.generation_rx.changed() => {
                    if changed.is_err() { Action::Disconnect } else {
                        // New block, target, root or flags: every miner starts over (`flag_newblock`).
                        self.mining = false;
                        self.ready = false;
                        Action::Continue
                    }
                }
                _ = keepalive => {
                    // Never re-notify a job the node has already built past.
                    if self.mining && !state.lock().template_stale() {
                        if let Some(w) = self.jobs.back() {
                            out.push_str(&msg_notify(w, false));
                            debug!("miner {}: keepalive re-notify job {}", self.index, w.job_id);
                        }
                    }
                    self.last_write = Instant::now();
                    Action::Continue
                }
                _ = silence => {
                    if self.auth {
                        warn!("miner {}: silent for {} keepalive periods, disconnecting", self.index, state.limits.idle_keepalives);
                    } else {
                        warn!("miner {}: no mining.authorize within {:?}, disconnecting", self.index, state.limits.auth_timeout);
                    }
                    Action::Disconnect
                }
            };
            if let Action::Continue = action {
                self.pump(&state, &mut out);
            }
            if !out.is_empty() {
                if wr.write_all(out.as_bytes()).await.is_err() {
                    break;
                }
                self.last_write = Instant::now();
                out.clear();
            }
            if let Action::Disconnect = action {
                let _ = wr.shutdown().await;
                break;
            }
        }
        self.auth
    }

    /// The Perl main loop's per-client branch: set the target for a miner that needs one, then
    /// hand out work to a miner that is authorized, targeted and idle.
    fn pump(&mut self, state: &State, out: &mut String) {
        if !self.auth {
            return;
        }
        let g = state.lock();
        let Some(template) = g.template.as_ref() else { return };
        if g.template_stale() {
            // The node accepted a block at this height; wait for the poller's next template
            // (it has been woken) rather than hand out work that can only come back
            // `inconclusive`.
            return;
        }
        if !self.ready {
            out.push_str(&msg_set_target(&template.target));
            self.ready = true;
        }
        if !self.mining {
            // One job per client, numbered by a per-client counter (`stratumpool`'s
            // `$client->{'worknumber'}`; the `stratumsolo` global job is gone with the modes, Y7).
            self.work_number += 1;
            let params = BuildParams { job_id: self.work_number.to_string(), miner_script_pubkey: self.script_pubkey.as_deref(), now: job_time(template) };
            let work = match build_work(template, &state.policy, state.equihash, &params) {
                Ok(w) => Some(w),
                Err(e) => {
                    warn!("miner {}: cannot build work: {}", self.index, e);
                    None
                }
            };
            drop(g);
            if let Some(w) = work {
                out.push_str(&msg_notify(&w, true));
                debug!("miner {}: job {} height {} tag {}", self.index, w.job_id, w.height, w.tag_kind());
                self.jobs.push_back(w);
                while self.jobs.len() > JOBS_KEPT {
                    self.jobs.pop_front();
                }
                self.mining = true;
            }
        }
    }

    /// Marks the miner authorized (once; the gauge counts authorized miners only).
    fn authorized(&mut self, state: &State) {
        if !self.auth {
            self.auth = true;
            state.lock().miners += 1;
        }
    }

    async fn handle_line(&mut self, line: &str, state: &State, out: &mut String) -> Action {
        let line = line.trim();
        if line.is_empty() {
            return Action::Continue;
        }
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                warn!("miner {}: garbage from {}, disconnecting", self.index, self.peer);
                return Action::Disconnect;
            }
        };
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let params = req.get("params").and_then(Value::as_array).cloned().unwrap_or_default();
        let param = |i: usize| params.get(i).and_then(Value::as_str).unwrap_or("").to_string();
        match req.get("method").and_then(Value::as_str).unwrap_or("") {
            "mining.subscribe" => {
                self.software = param(0);
                out.push_str(&msg_subscribe(&id, &self.nonce1));
                info!("miner {}: subscribed ({}), nonce1 {}", self.index, shown(&self.software), self.nonce1);
                Action::Continue
            }
            "mining.authorize" => {
                // Audit H-4: one authorize per connection, a per-IP budget, cached answers.
                if self.auth {
                    warn!("miner {}: second mining.authorize, disconnecting", self.index);
                    return Action::Disconnect;
                }
                if !state.authorize_allowed(self.peer.ip()) {
                    out.push_str(&msg_auth_failed(&id, "Auth Failed"));
                    warn!("miner {}: too many mining.authorize from {}, disconnecting", self.index, self.peer.ip());
                    return Action::Disconnect;
                }
                self.worker = param(0);
                let password = param(1);
                if let Some(expected) = &state.password {
                    // Audit H-5: constant-time compare.
                    if !bool::from(password.as_bytes().ct_eq(expected.as_bytes())) {
                        out.push_str(&msg_auth_failed(&id, "Auth Failed"));
                        warn!("miner {}: wrong password for {}", self.index, shown(&self.worker));
                        return Action::Disconnect;
                    }
                }
                if let Some(fixed) = &state.policy.payout {
                    // --payout: every block pays the fixed address; the username is a worker name.
                    self.authorized(state);
                    out.push_str(&msg_authorized(&id));
                    info!("miner {}: authorized as {}, paying {}", self.index, shown(&self.worker), fixed.address);
                    return Action::Continue;
                }
                // no --payout: the username is the payout address
                let cached = state.cached_address(&self.worker);
                let was_cached = cached.is_some();
                let answer = match cached {
                    Some(spk) => Ok(spk),
                    None => {
                        let rpc = state.rpc.clone();
                        let address = self.worker.clone();
                        match tokio::task::spawn_blocking(move || rpc.validateaddress(&address)).await {
                            Ok(Ok(v)) if v.isvalid => match v.script_pubkey.as_deref().map(hex::decode) {
                                Some(Ok(spk)) => Ok(Some(spk)),
                                _ => {
                                    warn!("miner {}: {} validates but has no scriptPubKey (shielded?)", self.index, shown(&self.worker));
                                    Ok(None)
                                }
                            },
                            Ok(Ok(_)) => {
                                warn!("miner {}: invalid payout address {}", self.index, shown(&self.worker));
                                Ok(None)
                            }
                            Ok(Err(e)) => Err(format!("validateaddress failed: {}", e)),
                            Err(e) => Err(format!("validateaddress task: {}", e)),
                        }
                    }
                };
                match answer {
                    Ok(Some(spk)) => {
                        if !was_cached {
                            state.cache_address(&self.worker, Some(spk.clone()));
                        }
                        self.script_pubkey = Some(spk);
                        self.authorized(state);
                        out.push_str(&msg_authorized(&id));
                        info!("miner {}: authorized, paying {}", self.index, shown(&self.worker));
                        Action::Continue
                    }
                    Ok(None) => {
                        if !was_cached {
                            state.cache_address(&self.worker, None);
                        }
                        out.push_str(&msg_auth_failed(&id, "Invalid address"));
                        Action::Disconnect
                    }
                    Err(e) => {
                        out.push_str(&msg_auth_failed(&id, "Invalid address"));
                        warn!("miner {}: {}", self.index, e);
                        Action::Disconnect
                    }
                }
            }
            "mining.extranonce.subscribe" => {
                // Acknowledged only, as `stratumpool` and `cenote` do. (`stratumsolo:101-105`
                // re-issued the target and the same job here; dropped with the modes, Y7.)
                out.push_str(&msg_extranonce(&id));
                Action::Continue
            }
            "mining.submit" => {
                if !self.auth {
                    warn!("miner {}: submit before authorize, disconnecting", self.index);
                    return Action::Disconnect;
                }
                self.mining = false;
                self.ready = false;
                let ok = self.submit(&id, &params, state).await;
                out.push_str(&msg_submit_result(&id, ok));
                if self.bad_submits >= state.limits.max_bad_submits {
                    warn!("miner {}: {} bad submits, disconnecting", self.index, self.bad_submits);
                    return Action::Disconnect;
                }
                Action::Continue
            }
            other => {
                warn!("miner {}: unknown method {}, disconnecting", self.index, shown(other));
                Action::Disconnect
            }
        }
    }

    /// `mining.submit` [worker, job id, ntime, nonce2, solution] → true only when the node
    /// returns null; the verdict string is logged and kept for `/status`. A submit that fails
    /// the pool's own checks (shape, unknown job, hash above target) never reaches the node
    /// (audit H-3, H-17).
    async fn submit(&mut self, _id: &Value, params: &[Value], state: &State) -> bool {
        let p = |i: usize| params.get(i).and_then(Value::as_str).unwrap_or("");
        let job_id = p(1);
        let Some(work) = self.jobs.iter().rev().find(|w| w.job_id == job_id).cloned() else {
            warn!("miner {}: submit for unknown job {}", self.index, shown(job_id));
            self.record(state, "stale");
            return false;
        };
        let submit = match Submit::check(p(2), p(3), p(4), self.nonce1.len(), state.equihash) {
            Ok(s) => s,
            Err(e) => {
                warn!("miner {}: bad submit for job {}: {}", self.index, job_id, e);
                self.bad_submits += 1;
                self.record(state, "bad-submit");
                return false;
            }
        };
        let header = assemble_header(&work, &self.nonce1, &submit);
        match if state.limits.check_pow { meets_target(&header, &work.target) } else { Ok(true) } {
            Ok(true) => {}
            Ok(false) => {
                warn!("miner {}: submit for job {} is above the target, not forwarded", self.index, job_id);
                self.bad_submits += 1;
                self.record(state, "high-hash");
                return false;
            }
            Err(e) => {
                warn!("miner {}: cannot hash submit for job {}: {}", self.index, job_id, e);
                self.bad_submits += 1;
                self.record(state, "bad-submit");
                return false;
            }
        }
        self.bad_submits = 0;
        let block = assemble_block(&work, &self.nonce1, &submit);
        let rpc = state.rpc.clone();
        // Audit H-3: at most `submits_in_flight` submitblock calls at once, pool-wide.
        let _permit = state.submit_permits.acquire().await;
        let verdict = tokio::task::spawn_blocking(move || rpc.submitblock(&block)).await;
        match verdict {
            Ok(Ok(None)) => {
                info!("miner {}: block {} accepted by the node (job {}, tag {})", self.index, work.height, job_id, work.tag_kind());
                state.block_accepted(&work.previousblockhash);
                true
            }
            Ok(Ok(Some(verdict))) => {
                warn!("miner {}: block {} rejected by the node: {}", self.index, work.height, verdict);
                self.record(state, &verdict);
                false
            }
            Ok(Err(e)) => {
                warn!("miner {}: submitblock failed: {}", self.index, e);
                self.record(state, &format!("rpc-error: {}", e));
                false
            }
            Err(e) => {
                warn!("miner {}: submitblock task: {}", self.index, e);
                self.record(state, "task-error");
                false
            }
        }
    }

    fn record(&self, state: &State, verdict: &str) {
        let mut g = state.lock();
        g.rejected += 1;
        g.last_verdict = verdict.to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce1_is_client_index_plus_random() {
        let n = make_nonce1(0x1234);
        assert_eq!(n.len(), NONCE1_HEX_LEN);
        assert_eq!(n.len(), 28);
        assert!(n.starts_with("1234"));
        assert!(n.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        assert_ne!(make_nonce1(1), make_nonce1(1));
        assert!(make_nonce1(0x1_0005).starts_with("0005"));
    }

    #[test]
    fn miner_strings_are_escaped_and_cut() {
        assert_eq!(shown("gminer"), "\"gminer\"");
        assert_eq!(shown("a\nblock 9 accepted\x1b[0m"), "\"a\\nblock 9 accepted\\u{1b}[0m\"");
        let long = "é".repeat(100);
        let cut = shown(&long);
        assert!(cut.ends_with("…"));
        assert!(cut.len() < 140);
        assert_eq!(shown(&"x".repeat(128)), format!("{:?}", "x".repeat(128)));
    }

    #[tokio::test]
    async fn line_reader_is_capped() {
        let mut buf = Vec::new();
        let mut r = BufReader::new(&b"one\r\ntwo\nthree"[..]);
        assert_eq!(read_line_capped(&mut r, &mut buf).await.unwrap().as_deref(), Some("one"));
        assert_eq!(read_line_capped(&mut r, &mut buf).await.unwrap().as_deref(), Some("two"));
        assert_eq!(read_line_capped(&mut r, &mut buf).await.unwrap().as_deref(), Some("three"));
        assert_eq!(read_line_capped(&mut r, &mut buf).await.unwrap(), None);
        let ok = "x".repeat(MAX_LINE - 1) + "\n";
        let mut r = BufReader::new(ok.as_bytes());
        assert_eq!(read_line_capped(&mut r, &mut buf).await.unwrap().unwrap().len(), MAX_LINE - 1);
        let over = "x".repeat(MAX_LINE) + "\n";
        let mut r = BufReader::new(over.as_bytes());
        let e = read_line_capped(&mut r, &mut buf).await.unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
        // no newline at all: the error comes once the cap is hit, not at EOF
        let endless = "y".repeat(MAX_LINE * 3);
        let mut r = BufReader::with_capacity(100, endless.as_bytes());
        assert!(read_line_capped(&mut r, &mut buf).await.is_err());
        assert!(buf.len() <= MAX_LINE);
    }

    #[test]
    fn messages_match_the_perl_byte_for_byte() {
        let id = serde_json::json!(7);
        assert_eq!(msg_subscribe(&id, "000094e6b550edb9bd052ceac8af"), "{\"id\":7,\"result\":[null,\"000094e6b550edb9bd052ceac8af\"],\"error\":null}\n");
        assert_eq!(msg_authorized(&id), "{\"id\":7,\"result\": true,\"error\": null}\n");
        assert_eq!(msg_auth_failed(&id, "Auth Failed"), "{\"id\":7,\"result\": false,\"error\": \"Auth Failed\"}\n");
        assert_eq!(msg_extranonce(&id), "{\"id\":7,\"result\": true,\"error\": null}\n");
        assert_eq!(
            msg_set_target("0f0f0f0000000000000000000000000000000000000000000000000000000000"),
            "{\"id\":null,\"method\":\"mining.set_target\",\"params\":[\"0f0f0f0000000000000000000000000000000000000000000000000000000000\"]}\n"
        );
        assert_eq!(msg_submit_result(&id, true), "{\"id\":7,\"result\": true}\n");
        assert_eq!(msg_submit_result(&id, false), "{\"id\":7,\"result\": false}\n");
        assert_eq!(msg_submit_result(&serde_json::json!("x"), false), "{\"id\":\"x\",\"result\": false}\n");
        let w = Work {
            job_id: "3".into(),
            height: 105,
            version: "04000000".into(),
            previousblockhash: "aa".repeat(32),
            merkleroot: "bb".repeat(32),
            light_client_root: "cc".repeat(32),
            time: "b61cba6a".into(),
            bits: "0f0f0f20".into(),
            target: String::new(),
            transactions: String::new(),
            transaction_count: 1,
            coinbase_script_sig: vec![],
            tag: None,
            text_truncated_to: None,
        };
        assert_eq!(
            msg_notify(&w, true),
            format!(
                "{{\"id\":null,\"method\":\"mining.notify\",\"params\":[\"3\",\"04000000\",\"{}\",\"{}\",\"{}\",\"b61cba6a\",\"0f0f0f20\",true,\"ZcashPoW\"]}}\n",
                "aa".repeat(32),
                "bb".repeat(32),
                "cc".repeat(32)
            )
        );
        assert!(msg_notify(&w, false).contains("\"0f0f0f20\",false,\"ZcashPoW\""));
    }
}
