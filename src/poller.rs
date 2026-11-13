// Copyright (c) 2026 The Ycash developers
// Distributed under the MIT software license, see the accompanying
// file LICENSE or https://www.opensource.org/licenses/mit-license.php .

//! The template poller: `getblocktemplate` every second (`stratumsolo:178`), new work when
//! the change key moves (height, target, light-client root, `coinbaseaux.flags`: Y-F2), a
//! 5 s back-off while the node is down, a forced refresh when it comes back
//! (`restart_miners`), and an immediate fetch after an accepted `submitblock` (`State::refresh`)
//! so miners are not handed the template the node has just built past.

use std::time::{Duration, Instant};

use tracing::{debug, error, info, warn};

use crate::rpc::{RpcClient, RpcError};
use crate::state::State;
use crate::template::BlockTemplate;
use crate::work::{build_work, BuildParams};

pub fn now_unix() -> u32 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as u32).unwrap_or(0)
}

/// The job's nTime: the template's `curtime`, which the node already keeps inside both header
/// time rules (ycash6 `src/miner.cpp` `UpdateTime`, ycash-dd the same): at least
/// median-time-past + 1, so a burst-generated regtest chain whose median-time-past runs ahead of
/// the clock is not `time-too-old` (Y-F5), and at most median-time-past + 3 hours
/// (`MAX_FUTURE_BLOCK_TIME_MTP`), so a chain that has been idle for longer than that is not
/// `time-too-far-ahead-of-mtp` (Y-F10). The Perl sent wall-clock `time` (`stratumsolo:434`)
/// and the earlier `max(curtime, now)` broke the second rule. The bits the node serves are the
/// ones for `curtime` (on a testnet, the minimum-difficulty rule depends on it), so the header
/// time must be that one. Wall-clock time only when the node sends no `curtime`.
pub fn job_time(t: &BlockTemplate) -> u32 {
    t.curtime.and_then(|c| u32::try_from(c).ok()).unwrap_or_else(now_unix)
}

/// Applies a freshly fetched template; returns true when miners need new work.
pub fn apply_template(state: &State, template: BlockTemplate) -> bool {
    let key = template.change_key();
    let mut g = state.lock();
    let was_down = !g.node_up;
    g.node_up = true;
    if g.template.is_some() && g.key == key && !was_down {
        g.template = Some(template);
        g.template_at = Some(Instant::now());
        return false;
    }
    let reason = if was_down {
        "node back"
    } else if g.key.height != key.height {
        "new height"
    } else if g.key.flags != key.flags {
        "coinbaseaux.flags changed"
    } else if g.key.target != key.target {
        "target changed"
    } else {
        "light-client root changed"
    };
    g.key = key;
    g.template = Some(template);
    g.template_at = Some(Instant::now());
    g.generation += 1;
    let generation = g.generation;
    // Log the tag on every template change by building a probe job under the policy (a
    // placeholder payout script stands in for the username when there is no `--payout`).
    let t = g.template.clone().unwrap();
    let probe = BuildParams { job_id: "probe".into(), miner_script_pubkey: Some(&[0x76, 0xa9, 0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x88, 0xac]), now: job_time(&t) };
    match build_work(&t, &state.policy, state.equihash, &probe) {
        Ok(w) => {
            g.last_tag_kind = w.tag_kind();
            match &w.tag {
                Some(tag) => info!(
                    "work {} (gen {}, {}): {} txs, tag: {} price={} µUSD mask={:#x} signal={} payout={} scriptSig={}B",
                    t.height,
                    generation,
                    reason,
                    w.transaction_count,
                    tag.kind(),
                    tag.price_micro_usd,
                    tag.source_mask,
                    tag.signal,
                    hex::encode(tag.payout_key),
                    w.coinbase_script_sig.len()
                ),
                None => info!("work {} (gen {}, {}): {} txs, tag: none scriptSig={}B", t.height, generation, reason, w.transaction_count, w.coinbase_script_sig.len()),
            }
            if let Some(used) = w.text_truncated_to {
                warn!("coinbase text truncated to {} bytes to keep the scriptSig within 100 bytes", used);
            }
        }
        Err(e) => error!("template {}: {}", t.height, e),
    }
    drop(g);
    let _ = state.generation_tx.send(generation);
    true
}

/// `getblocktemplate` on a thread of its own, not tokio's shared blocking pool: a flood of
/// `submitblock` / `validateaddress` calls queued there must never delay the template
/// refresh every other miner waits on (audit H-3). One short-lived thread per poll is cheap
/// next to the RPC round trip.
async fn fetch_template(rpc: RpcClient) -> Result<Result<serde_json::Value, RpcError>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("yolo-gbt".into())
        .spawn(move || {
            let _ = tx.send(rpc.getblocktemplate());
        })
        .map_err(|e| format!("cannot spawn the template thread: {}", e))?;
    rx.await.map_err(|_| "template thread died".to_string())
}

pub async fn run(state: State) {
    let mut down_logged = false;
    loop {
        let fetched = fetch_template(state.rpc.clone()).await;
        match fetched {
            Ok(Ok(v)) => match serde_json::from_value::<BlockTemplate>(v) {
                Ok(t) => {
                    down_logged = false;
                    if !apply_template(&state, t) {
                        debug!("template unchanged");
                    }
                    // A template at the height the node has just accepted is stale: the node
                    // has not rolled its template yet, so ask again soon instead of in 1 s.
                    let wait = if state.lock().template_stale() { Duration::from_millis(100) } else { Duration::from_secs(1) };
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        _ = state.refresh.notified() => debug!("template refresh requested"),
                    }
                }
                Err(e) => {
                    error!("getblocktemplate: cannot read template: {}", e);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            },
            Ok(Err(e)) => {
                state.lock().node_up = false;
                if !down_logged {
                    warn!("node is down ({}), retrying every 5 seconds", e);
                    down_logged = true;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            Err(e) => {
                error!("poller task failed: {}", e);
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template(curtime: Option<i64>) -> BlockTemplate {
        let mut t: BlockTemplate = serde_json::from_str(include_str!("../tests/vectors/regtest-template-105.json")).unwrap();
        t.curtime = curtime;
        t
    }

    /// testnet-yb-fast block 510,432 (2026-11-13): the chain had been idle for two days, so the
    /// node served `curtime` = median-time-past + 3 h, well before the wall clock. The header must
    /// carry that time; the wall clock is `time-too-far-ahead-of-mtp` (Y-F10).
    #[test]
    fn job_time_is_the_node_curtime_even_far_behind_the_clock() {
        let curtime = 1_794_402_051;
        assert!(now_unix() > curtime + 3 * 3600);
        assert_eq!(job_time(&template(Some(curtime as i64))), curtime);
    }

    /// Y-F5: a regtest chain whose median-time-past runs ahead of the clock: `curtime` is ahead too.
    #[test]
    fn job_time_ahead_of_the_clock_is_kept() {
        let ahead = now_unix() + 3600;
        assert_eq!(job_time(&template(Some(ahead as i64))), ahead);
    }

    #[test]
    fn job_time_without_curtime_is_the_clock() {
        let before = now_unix();
        let t = job_time(&template(None));
        assert!(t >= before && t <= now_unix());
    }
}
