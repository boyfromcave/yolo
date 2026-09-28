//! Shared server state: the current template, the counters `/status` reports, and the
//! generation number that tells clients to fetch new work.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::{watch, Notify};

use crate::equihash::Equihash;
use crate::rpc::RpcClient;
use crate::template::{BlockTemplate, ChangeKey};
use crate::work::Policy;

pub struct Inner {
    pub template: Option<BlockTemplate>,
    pub key: ChangeKey,
    pub template_at: Option<Instant>,
    pub generation: u64,
    pub node_up: bool,
    pub miners: usize,
    pub clients_seen: u64,
    pub last_tag_kind: &'static str,
    pub last_verdict: String,
    pub accepted: u64,
    pub rejected: u64,
    /// The `previousblockhash` (wire order, as `Work` carries it) of the last job the node
    /// accepted a block for: a template still on that parent is stale (the node has moved on)
    /// and no work is built from it until the poller brings the next one — otherwise a fast
    /// solver re-solves the old job and every submit comes back `inconclusive`.
    pub accepted_parent: Option<String>,
}

impl Inner {
    /// True while the current template is one the node has already built on.
    pub fn template_stale(&self) -> bool {
        match (&self.template, &self.accepted_parent) {
            (Some(t), Some(parent)) => crate::codec::reverse_hex(&t.previousblockhash) == *parent,
            _ => false,
        }
    }
}

pub struct Shared {
    pub inner: Mutex<Inner>,
    pub generation_tx: watch::Sender<u64>,
    /// Wakes the poller before its next 1 s tick (after an accepted `submitblock`).
    pub refresh: Notify,
    pub rpc: RpcClient,
    pub policy: Policy,
    pub equihash: Equihash,
    pub password: Option<String>,
    pub started: Instant,
}

pub type State = Arc<Shared>;

impl Shared {
    pub fn new(rpc: RpcClient, policy: Policy, equihash: Equihash, password: Option<String>) -> (State, watch::Receiver<u64>) {
        let (generation_tx, rx) = watch::channel(0);
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                template: None,
                key: ChangeKey::default(),
                template_at: None,
                generation: 0,
                node_up: false,
                miners: 0,
                clients_seen: 0,
                last_tag_kind: "none",
                last_verdict: String::new(),
                accepted: 0,
                rejected: 0,
                accepted_parent: None,
            }),
            generation_tx,
            refresh: Notify::new(),
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

    /// Records an accepted block and asks the poller for the next template now.
    pub fn block_accepted(&self, parent: &str) {
        let mut g = self.lock();
        g.accepted += 1;
        g.last_verdict = "accepted".into();
        g.accepted_parent = Some(parent.to_string());
        drop(g);
        self.refresh.notify_one();
    }

    /// The `/status` document.
    pub fn status_json(&self) -> serde_json::Value {
        let g = self.lock();
        serde_json::json!({
            "payout": self.policy.payout_label(),
            "text": self.policy.text.is_some(),
            "equihash": self.equihash.to_string(),
            "nodeUp": g.node_up,
            "height": g.template.as_ref().map(|t| t.height),
            "templateAgeSeconds": g.template_at.map(|t| t.elapsed().as_secs_f64().round()),
            "miners": g.miners,
            "tag": g.last_tag_kind,
            "lastSubmitVerdict": g.last_verdict,
            "accepted": g.accepted,
            "rejected": g.rejected,
            "uptimeSeconds": self.started.elapsed().as_secs(),
        })
    }
}
