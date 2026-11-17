// Copyright (c) 2026 The Ycash developers
// Distributed under the MIT software license, see the accompanying
// file LICENSE or https://www.opensource.org/licenses/mit-license.php .

//! Block pacing (`--min-block-interval`, off by default, refused on mainnet): the pool submits a
//! block no sooner than the interval after it read that block's job time from the node. A block
//! found sooner is held; the miner is answered at once and keeps mining.
//!
//! Why this spaces the chain's timestamps, which is all the difficulty rule reads: a job's header
//! time is the node's `curtime` of the template it was built from (`poller::job_time`), and the
//! pool builds no job on a block before the node has accepted that block. So a block with time
//! `t` is published at `t + interval` or later, every job on top of it is read after that, and the
//! next block's time is at least `t + interval`. Each timestamp is one interval older than its
//! block's publication, and at least one interval after its parent's timestamp.
//!
//! Which block is held: at the lowest difficulty a card has a block within a second of its job,
//! so "the first one" would always be the miner whose job arrives first, and a Sapling or
//! post-quantum miner, whose job waits for the node's coinbase, would never win. Every verified
//! block-target solution received during the selection window (the first sixth of the interval
//! on a tip) is a candidate, and the k-th takes the held one's place with probability 1/k: each
//! candidate is equally likely to be the block. The held block is always its finder's own, so
//! pacing never changes who is paid or how much.
//!
//! A held block is dropped, never submitted, when the pool sees another parent before the
//! release (another miner's block arrived). Everything here is plain state under the pool's
//! lock; `run` is the one task that waits and calls `submitblock`.

use std::time::{Duration, Instant};

use tracing::{info, warn};

use crate::state::State;

/// The selection window is the interval divided by this (60 s → 10 s), at least one second.
pub const SELECTION_DIVISOR: u32 = 6;
/// The largest interval the flag takes, in seconds. The fast testnet's target spacing is 75 s;
/// nothing above a few times that is useful, and the value stays below the 450 s after which a
/// testnet block may be mined at the minimum difficulty.
pub const MAX_INTERVAL_SECS: u32 = 300;

/// Refuses pacing on mainnet: a block held there is lost to every other miner.
pub fn check_chain(chain: &str, interval: Duration) -> Result<(), String> {
    if !interval.is_zero() && chain == "main" {
        return Err("--min-block-interval is refused on mainnet (chain \"main\"): a held block would be lost to any other miner; it is a testnet and regtest setting".into());
    }
    Ok(())
}

/// A block waiting for its release.
#[derive(Debug, Clone)]
pub struct Held {
    /// The whole block, hex, as `submitblock` takes it.
    pub block: String,
    /// Its parent, as `Work::previousblockhash` writes it.
    pub parent: String,
    pub height: u32,
    /// The finder's connection number and job, for the log.
    pub miner: u64,
    pub job_id: String,
    pub tag: &'static str,
    /// When the pool may submit it: its job's read time plus the interval.
    pub due: Instant,
}

/// What became of an offered block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offer {
    /// Its job is already one interval old and nothing is held: submit it now, as without pacing.
    SubmitNow,
    /// It is the held block now (`replaced`: it took an earlier candidate's place).
    Held { replaced: bool },
    /// Another candidate stays the held block; this one is credited to its miner as a share.
    NotChosen,
}

#[derive(Debug)]
pub struct Pacer {
    interval: Duration,
    /// The parent the pool mines on and when the poller first saw it.
    tip: Option<(String, Instant)>,
    held: Option<Held>,
    /// Candidates on this tip so far.
    candidates: u64,
    /// Blocks held on a tip the chain moved away from: dropped, never submitted.
    pub held_stale: u64,
    /// Block-target solutions answered as a candidate (held or not) since start.
    pub candidates_total: u64,
}

impl Pacer {
    pub fn new(interval: Duration) -> Pacer {
        Pacer { interval, tip: None, held: None, candidates: 0, held_stale: 0, candidates_total: 0 }
    }

    pub fn enabled(&self) -> bool {
        !self.interval.is_zero()
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// How long after a tip is first seen candidates still compete for the held block.
    pub fn selection_window(&self) -> Duration {
        (self.interval / SELECTION_DIVISOR).max(Duration::from_secs(1)).min(self.interval)
    }

    pub fn held(&self) -> Option<&Held> {
        self.held.as_ref()
    }

    pub fn candidates(&self) -> u64 {
        self.candidates
    }

    /// The poller's every template: a parent other than the current one starts a new tip and
    /// drops the held block, which is returned for the log.
    pub fn see_tip(&mut self, parent: &str, now: Instant) -> Option<Held> {
        if self.tip.as_ref().is_some_and(|(p, _)| p == parent) {
            return None;
        }
        self.tip = Some((parent.to_string(), now));
        self.candidates = 0;
        let dropped = self.held.take();
        if dropped.is_some() {
            self.held_stale += 1;
        }
        dropped
    }

    /// A verified block-target solution on `parent`, from a job whose time was read at `issued`.
    /// `draw(k)` is uniform in `0..k`; `make` builds the block only when it is to be held.
    pub fn offer(&mut self, parent: &str, issued: Instant, now: Instant, draw: impl FnOnce(u64) -> u64, make: impl FnOnce(Instant) -> Held) -> Offer {
        if !self.enabled() {
            return Offer::SubmitNow;
        }
        // The caller has just checked that `parent` is the template's; the poller normally told us first.
        self.see_tip(parent, now);
        let due = issued + self.interval;
        if self.held.is_none() {
            if now >= due {
                return Offer::SubmitNow;
            }
            self.candidates += 1;
            self.candidates_total += 1;
            self.held = Some(make(due));
            return Offer::Held { replaced: false };
        }
        self.candidates_total += 1;
        let first_seen = self.tip.as_ref().map(|(_, at)| *at).unwrap_or(now);
        if now >= first_seen + self.selection_window() {
            return Offer::NotChosen;
        }
        self.candidates += 1;
        if draw(self.candidates) == 0 {
            self.held = Some(make(due));
            Offer::Held { replaced: true }
        } else {
            Offer::NotChosen
        }
    }

    /// The held block, once it is due.
    pub fn take_due(&mut self, now: Instant) -> Option<Held> {
        if self.held.as_ref().is_some_and(|h| now >= h.due) {
            return self.held.take();
        }
        None
    }

    /// Puts a block back after a `submitblock` that did not reach the node, to be tried again at
    /// `due`, unless the tip has changed or another block is held meanwhile.
    pub fn put_back(&mut self, mut held: Held, due: Instant) -> bool {
        if self.held.is_some() || self.tip.as_ref().is_none_or(|(p, _)| *p != held.parent) {
            return false;
        }
        held.due = due;
        self.held = Some(held);
        true
    }

    /// Time until the pool may next submit a block: the held block's release, else what is left
    /// of the interval since the tip was first seen (no job on this tip is older than that).
    pub fn next_block_in(&self, now: Instant) -> Duration {
        if !self.enabled() {
            return Duration::ZERO;
        }
        match (&self.held, &self.tip) {
            (Some(h), _) => h.due.saturating_duration_since(now),
            (None, Some((_, seen))) => (*seen + self.interval).saturating_duration_since(now),
            (None, None) => Duration::ZERO,
        }
    }

    /// The `held` object of `/status`.
    pub fn held_json(&self, now: Instant) -> serde_json::Value {
        match &self.held {
            Some(h) => serde_json::json!({
                "height": h.height,
                "miner": h.miner,
                "candidates": self.candidates,
                "releaseInSeconds": h.due.saturating_duration_since(now).as_secs_f64().ceil(),
            }),
            None => serde_json::Value::Null,
        }
    }
}

/// Uniform in `0..k` from the system's random source (`k` ≥ 1); 0 if it fails, which only makes
/// the newest candidate the held one.
pub fn draw(k: u64) -> u64 {
    let mut b = [0u8; 8];
    if getrandom::fill(&mut b).is_err() {
        return 0;
    }
    u64::from_le_bytes(b) % k.max(1)
}

/// The release task: sleeps until the held block is due (woken when one is held), then submits
/// it if the pool is still mining on its parent.
pub async fn run(state: State) {
    if !state.lock().pacer.enabled() {
        return;
    }
    loop {
        let due = state.lock().pacer.held().map(|h| h.due);
        match due {
            None => state.pace_wake.notified().await,
            Some(due) => {
                tokio::select! {
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(due)) => {}
                    _ = state.pace_wake.notified() => continue,
                }
            }
        }
        let held = {
            let mut g = state.lock();
            let Some(h) = g.pacer.take_due(Instant::now()) else { continue };
            // Never submit on a stale tip: the template must still be on this parent and must not
            // be one the node has already built past.
            let on_tip = g.template.as_ref().is_some_and(|t| crate::codec::reverse_hex(&t.previousblockhash) == h.parent) && !g.template_stale();
            if !on_tip {
                g.pacer.held_stale += 1;
                warn!("held block {} (miner {}, job {}) dropped at its release: the chain has moved past its parent", h.height, h.miner, h.job_id);
                continue;
            }
            h
        };
        let rpc = state.rpc.clone();
        let block = held.block.clone();
        let _permit = state.submit_permits.acquire().await;
        match tokio::task::spawn_blocking(move || rpc.submitblock(&block)).await {
            Ok(Ok(None)) => {
                info!("held block {} accepted by the node (miner {}, job {}, tag {})", held.height, held.miner, held.job_id, held.tag);
                state.block_accepted(&held.parent);
            }
            Ok(Ok(Some(verdict))) => {
                warn!("held block {} (miner {}, job {}) rejected by the node: {}", held.height, held.miner, held.job_id, verdict);
                let mut g = state.lock();
                if crate::stratum::RACE_VERDICTS.contains(&verdict.as_str()) {
                    g.pacer.held_stale += 1;
                } else {
                    g.rejected += 1;
                }
                g.last_verdict = verdict;
            }
            Ok(Err(e)) => {
                warn!("held block {}: submitblock failed: {}; trying again in a second", held.height, e);
                if !state.lock().pacer.put_back(held, Instant::now() + Duration::from_secs(1)) {
                    warn!("the block is dropped: the chain has moved on");
                }
            }
            Err(e) => warn!("held block {}: submitblock task: {}", held.height, e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const N: Duration = Duration::from_secs(60);

    fn held(miner: u64) -> impl FnOnce(Instant) -> Held {
        move |due| Held { block: format!("block-of-{}", miner), parent: "p1".into(), height: 7, miner, job_id: "1".into(), tag: "none", due }
    }

    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn mainnet_is_refused_and_only_mainnet() {
        assert!(check_chain("main", N).unwrap_err().contains("refused on mainnet"));
        assert!(check_chain("main", Duration::ZERO).is_ok(), "off is allowed everywhere");
        for chain in ["test", "regtest", "testnet-yb-fast", "testnet-yb-slow"] {
            assert!(check_chain(chain, N).is_ok(), "{}", chain);
        }
    }

    #[test]
    fn off_submits_everything_at_once() {
        let t0 = Instant::now();
        let mut p = Pacer::new(Duration::ZERO);
        assert!(!p.enabled());
        assert_eq!(p.offer("p1", t0, t0, |_| 0, held(1)), Offer::SubmitNow);
        assert!(p.held().is_none());
        assert_eq!(p.next_block_in(t0), Duration::ZERO);
    }

    /// The release is counted from the job, to the second: one interval after its time was read.
    #[test]
    fn a_block_is_held_until_its_job_is_one_interval_old() {
        let t0 = Instant::now();
        let mut p = Pacer::new(N);
        assert!(p.see_tip("p1", t0).is_none());
        assert_eq!(p.next_block_in(t0 + s(10)), s(50), "no block yet: the rest of the interval on this tip");
        // a job read 3 s after the tip was seen, solved 2 s later
        assert_eq!(p.offer("p1", t0 + s(3), t0 + s(5), |_| 0, held(1)), Offer::Held { replaced: false });
        assert_eq!(p.held().unwrap().due, t0 + s(63));
        assert_eq!(p.next_block_in(t0 + s(5)), s(58));
        assert!(p.take_due(t0 + s(62)).is_none(), "a second early");
        assert!(p.take_due(t0 + s(60)).is_none(), "the tip's age does not release it: the job's does");
        let h = p.take_due(t0 + s(63)).expect("due");
        assert_eq!((h.miner, h.block.as_str()), (1, "block-of-1"));
        assert!(p.held().is_none());
    }

    #[test]
    fn a_solution_on_a_job_already_one_interval_old_is_submitted_at_once() {
        let t0 = Instant::now();
        let mut p = Pacer::new(N);
        p.see_tip("p1", t0);
        assert_eq!(p.offer("p1", t0 + s(1), t0 + s(61), |_| 0, held(1)), Offer::SubmitNow, "exactly one interval");
        assert!(p.held().is_none());
        // a young job on an old tip is held alone, and released by its own job's age
        assert_eq!(p.offer("p1", t0 + s(100), t0 + s(101), |_| 0, held(2)), Offer::Held { replaced: false });
        assert_eq!(p.offer("p1", t0 + s(1), t0 + s(102), |_| 0, held(3)), Offer::NotChosen, "the window is over: the held block stays");
        assert_eq!(p.take_due(t0 + s(160)).unwrap().miner, 2);
    }

    #[test]
    fn candidates_compete_only_inside_the_selection_window() {
        let t0 = Instant::now();
        let mut p = Pacer::new(N);
        assert_eq!(p.selection_window(), s(10));
        assert_eq!(Pacer::new(s(3)).selection_window(), s(1));
        assert_eq!(Pacer::new(Duration::from_millis(500)).selection_window(), Duration::from_millis(500));
        p.see_tip("p1", t0);
        assert_eq!(p.offer("p1", t0, t0 + s(1), |_| 0, held(1)), Offer::Held { replaced: false });
        assert_eq!(p.offer("p1", t0, t0 + s(2), |k| k - 1, held(2)), Offer::NotChosen, "the draw decides");
        assert_eq!(
            p.offer(
                "p1",
                t0 + s(4),
                t0 + s(5),
                |k| {
                    assert_eq!(k, 3);
                    0
                },
                held(3)
            ),
            Offer::Held { replaced: true }
        );
        assert_eq!(p.held().unwrap().due, t0 + s(64), "the release follows the held block's own job");
        assert_eq!(p.offer("p1", t0, t0 + s(10), |_| 0, held(4)), Offer::NotChosen, "at the window's end");
        assert_eq!((p.candidates(), p.candidates_total), (3, 4));
        assert_eq!(p.held().unwrap().miner, 3);
    }

    /// Over many windows each of k finders is the held one about 1/k of the time, whatever the
    /// order their solutions arrive in.
    #[test]
    fn each_of_k_candidates_is_held_equally_often() {
        // xorshift64*: a fixed seed, so the test is the same on every run
        let x = std::cell::Cell::new(0x9e37_79b9_7f4a_7c15u64);
        let next = |k: u64| {
            let mut v = x.get();
            v ^= v >> 12;
            v ^= v << 25;
            v ^= v >> 27;
            x.set(v);
            (v.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) % k
        };
        for k in [2u64, 4, 7] {
            let rounds = 40_000u64;
            let mut wins = vec![0u64; k as usize];
            let mut t = Instant::now();
            let mut p = Pacer::new(N);
            for round in 0..rounds {
                p.see_tip(&format!("p{}", round), t);
                // miner 0 is always first, the last always last
                for m in 0..k {
                    p.offer(&format!("p{}", round), t, t + Duration::from_millis(100 * (m + 1)), next, held(m));
                }
                wins[p.take_due(t + N).expect("one block a round").miner as usize] += 1;
                t += N;
            }
            let expected = rounds as f64 / k as f64;
            for (m, w) in wins.iter().enumerate() {
                let off = (*w as f64 - expected).abs() / expected;
                assert!(off < 0.05, "k = {}: miner {} held {} times, expected about {}", k, m, w, expected);
            }
        }
    }

    /// The tip changes under a held block: it is dropped and counted, and can never be taken.
    #[test]
    fn a_held_block_is_dropped_when_the_tip_changes() {
        let t0 = Instant::now();
        let mut p = Pacer::new(N);
        p.see_tip("p1", t0);
        assert_eq!(p.offer("p1", t0, t0 + s(1), |_| 0, held(1)), Offer::Held { replaced: false });
        assert!(p.see_tip("p1", t0 + s(2)).is_none(), "the same tip again changes nothing");
        assert_eq!(p.next_block_in(t0 + s(2)), s(58));
        let dropped = p.see_tip("p2", t0 + s(30)).expect("the held block comes back for the log");
        assert_eq!(dropped.miner, 1);
        assert_eq!(p.held_stale, 1);
        assert!(p.held().is_none());
        assert!(p.take_due(t0 + s(600)).is_none(), "nothing is ever released for the old tip");
        assert_eq!(p.candidates(), 0);
        assert_eq!(p.next_block_in(t0 + s(40)), s(50), "the new tip's interval runs from when it was seen");
        // a block that failed to reach the node is not put back on another tip
        assert!(!p.put_back(dropped, t0 + s(41)));
        assert!(p.held().is_none());
    }

    #[test]
    fn a_block_whose_submit_did_not_reach_the_node_is_tried_again() {
        let t0 = Instant::now();
        let mut p = Pacer::new(N);
        p.see_tip("p1", t0);
        p.offer("p1", t0, t0 + s(1), |_| 0, held(1));
        let h = p.take_due(t0 + s(60)).unwrap();
        assert!(p.put_back(h, t0 + s(61)));
        assert!(p.take_due(t0 + s(60)).is_none());
        assert_eq!(p.take_due(t0 + s(61)).unwrap().miner, 1);
    }

    #[test]
    fn status_shows_the_held_block() {
        let t0 = Instant::now();
        let mut p = Pacer::new(N);
        assert_eq!(p.held_json(t0), serde_json::Value::Null);
        p.see_tip("p1", t0);
        p.offer("p1", t0, t0 + s(1), |_| 0, held(9));
        assert_eq!(p.held_json(t0 + Duration::from_millis(1500)), serde_json::json!({ "height": 7, "miner": 9, "candidates": 1, "releaseInSeconds": 59.0 }));
    }

    #[test]
    fn draw_stays_in_range() {
        for k in 1..50 {
            assert!(draw(k) < k);
        }
        assert_eq!(draw(0), 0);
    }
}
