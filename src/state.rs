//! Shared server state: the current template, the counters `/status` reports, and the
//! generation number that tells clients to fetch new work.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::watch;

use crate::equihash::Equihash;
use crate::rpc::RpcClient;
use crate::template::{BlockTemplate, ChangeKey};
use crate::work::{Policy, Work};

pub struct Inner {
    pub template: Option<BlockTemplate>,
    pub key: ChangeKey,
    pub template_at: Option<Instant>,
    pub generation: u64,
    pub node_up: bool,
    /// The one job shared by every miner in solo mode (`stratumsolo`'s global `$work`).
    pub solo_work: Option<Work>,
    pub solo_work_number: u64,
    pub miners: usize,
    pub clients_seen: u64,
    pub last_tag_kind: &'static str,
    pub last_verdict: String,
    pub accepted: u64,
    pub rejected: u64,
    /// `--cenote N` blocks still to burn.
    pub cenote_left: u32,
}

pub struct Shared {
    pub inner: Mutex<Inner>,
    pub generation_tx: watch::Sender<u64>,
    pub rpc: RpcClient,
    pub policy: Policy,
    pub equihash: Equihash,
    pub password: Option<String>,
    pub started: Instant,
}

pub type State = Arc<Shared>;

impl Shared {
    pub fn new(rpc: RpcClient, policy: Policy, equihash: Equihash, password: Option<String>, cenote: u32) -> (State, watch::Receiver<u64>) {
        let (generation_tx, rx) = watch::channel(0);
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                template: None,
                key: ChangeKey::default(),
                template_at: None,
                generation: 0,
                node_up: false,
                solo_work: None,
                solo_work_number: 0,
                miners: 0,
                clients_seen: 0,
                last_tag_kind: "none",
                last_verdict: String::new(),
                accepted: 0,
                rejected: 0,
                cenote_left: cenote,
            }),
            generation_tx,
            rpc,
            policy,
            equihash,
            password,
            started: Instant::now(),
        });
        (shared, rx)
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The `/status` document.
    pub fn status_json(&self) -> serde_json::Value {
        let g = self.lock();
        serde_json::json!({
            "mode": self.policy.mode.to_string(),
            "equihash": self.equihash.to_string(),
            "nodeUp": g.node_up,
            "height": g.template.as_ref().map(|t| t.height),
            "templateAgeSeconds": g.template_at.map(|t| t.elapsed().as_secs_f64().round()),
            "miners": g.miners,
            "tag": g.last_tag_kind,
            "lastSubmitVerdict": g.last_verdict,
            "accepted": g.accepted,
            "rejected": g.rejected,
            "cenoteLeft": g.cenote_left,
            "uptimeSeconds": self.started.elapsed().as_secs(),
        })
    }
}
