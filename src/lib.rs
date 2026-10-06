// Copyright (c) 2026 The Ycash developers
// Distributed under the MIT software license, see the accompanying
// file LICENSE or https://www.opensource.org/licenses/mit-license.php .

//! yolo: a Ycash solo-pool stratum server, the Rust rewrite of `yecdev/yolo` (Perl
//! `stratumsolo`, `stratumpool`, `cenote`), aware of the Yellowback coinbase tag.
//!
//! The Perl is the behavioural and wire-format reference (`ref/yolo` in the workspace); see
//! `docs/plans/role-pool-regtest-plan.md` §3.2.

pub mod codec;
pub mod equihash;
pub mod poller;
pub mod rpc;
pub mod state;
pub mod status;
pub mod stratum;
pub mod tag;
pub mod template;
pub mod tx;
pub mod work;

use std::net::SocketAddr;

use tokio::net::TcpListener;
use tracing::info;

use equihash::{Equihash, EquihashArg};
use rpc::RpcClient;
pub use state::Limits;
use state::Shared;
use work::{Payout, Policy};

/// Everything `run` needs, as the CLI resolves it.
#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub status_bind: Option<SocketAddr>,
    /// `--payout`: resolved against the node's `validateaddress` in `bind`.
    pub payout: Option<String>,
    /// `--text`, `--no-flags`.
    pub text: Option<Vec<u8>>,
    pub no_flags: bool,
    pub password: Option<String>,
    pub equihash: EquihashArg,
    pub limits: Limits,
}

/// Resolves `--payout` to its scriptPubKey with `validateaddress`, retrying while the node is
/// unreachable; an invalid or shielded address is a startup error.
pub async fn resolve_payout(rpc: &RpcClient, address: &str) -> Result<Payout, String> {
    loop {
        let r = rpc.clone();
        let a = address.to_string();
        match tokio::task::spawn_blocking(move || r.validateaddress(&a)).await {
            Ok(Ok(v)) if v.isvalid => {
                let spk = v.script_pubkey.as_deref().map(hex::decode).transpose().map_err(|e| format!("--payout: scriptPubKey is not hex: {}", e))?;
                return match spk {
                    Some(script_pubkey) => Ok(Payout { address: address.to_string(), script_pubkey }),
                    None => Err(format!("--payout {}: not a transparent address (no scriptPubKey)", address)),
                };
            }
            Ok(Ok(_)) => return Err(format!("--payout {}: invalid address", address)),
            Ok(Err(e)) => tracing::warn!("validateaddress: {} (retrying in 5 s)", e),
            Err(e) => tracing::error!("validateaddress task: {}", e),
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// Resolves `--equihash auto` against the node, retrying while it is unreachable.
pub async fn resolve_equihash(rpc: &RpcClient, arg: EquihashArg) -> Equihash {
    if let EquihashArg::Fixed(e) = arg {
        return e;
    }
    loop {
        let r = rpc.clone();
        match tokio::task::spawn_blocking(move || r.getblockchaininfo()).await {
            Ok(Ok(info)) => {
                let chain = info.get("chain").and_then(|c| c.as_str()).unwrap_or("main");
                let e = Equihash::for_chain(chain);
                info!("chain {} → Equihash {}", chain, e);
                return e;
            }
            Ok(Err(e)) => tracing::warn!("getblockchaininfo: {} (retrying in 5 s)", e),
            Err(e) => tracing::error!("getblockchaininfo task: {}", e),
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// A bound but not yet serving pool: `addr` is known (useful with port 0), `serve` runs it.
pub struct Bound {
    pub addr: SocketAddr,
    pub status_addr: Option<SocketAddr>,
    listener: TcpListener,
    status_listener: Option<TcpListener>,
    state: state::State,
    generation_rx: tokio::sync::watch::Receiver<u64>,
}

impl Bound {
    pub fn state(&self) -> state::State {
        self.state.clone()
    }

    /// Serves until the task is dropped.
    pub async fn serve(self) {
        info!(
            "yolo {} listening on {} (payout {}, text {}, Equihash {}, node {})",
            env!("CARGO_PKG_VERSION"),
            self.addr,
            self.state.policy.payout_label(),
            if self.state.policy.text.is_some() { "set" } else { "node's" },
            self.state.equihash,
            self.state.rpc.url()
        );
        // The poller and the status server live exactly as long as this future: aborting or
        // dropping the serve task stops them too (they used to outlive it and keep polling).
        struct AbortOnDrop(Vec<tokio::task::JoinHandle<()>>);
        impl Drop for AbortOnDrop {
            fn drop(&mut self) {
                for h in &self.0 {
                    h.abort();
                }
            }
        }
        let mut tasks = AbortOnDrop(vec![tokio::spawn(poller::run(self.state.clone()))]);
        if let Some(l) = self.status_listener {
            info!("status on http://{}/status", self.status_addr.unwrap());
            tasks.0.push(tokio::spawn(status::serve(self.state.clone(), l)));
        }
        stratum::serve(self.state, self.listener, self.generation_rx).await;
        drop(tasks);
    }
}

fn policy_payout_is_username(payout: &Option<Payout>) -> bool {
    payout.is_none()
}

/// Resolves the Equihash parameters against the node and binds the listeners.
pub async fn bind(rpc: RpcClient, config: Config) -> Result<Bound, Box<dyn std::error::Error>> {
    let equihash = resolve_equihash(&rpc, config.equihash).await;
    let payout = match &config.payout {
        Some(a) => Some(resolve_payout(&rpc, a).await?),
        None => None,
    };
    if policy_payout_is_username(&payout) {
        // Audit H-7: the coinbase pays the miner, but the tag inside it (and so FEE-2
        // eligibility) stays with the node's -yellowbackpayoutaddress.
        tracing::warn!(
            "no --payout: each miner's username is paid, but the Yellowback tag's payoutKey is the node's \
             -yellowbackpayoutaddress regardless (miners are not registered as Yellowback miners)"
        );
    }
    if config.no_flags {
        tracing::warn!("--no-flags: every block will be mined WITHOUT the Yellowback tag (test switch)");
    }
    let policy = Policy { payout, text: config.text.clone(), no_flags: config.no_flags };
    let listener = TcpListener::bind(config.bind).await.map_err(|e| format!("cannot listen on {}: {}", config.bind, e))?;
    let addr = listener.local_addr()?;
    let (status_listener, status_addr) = match config.status_bind {
        Some(want) => {
            let l = TcpListener::bind(want).await.map_err(|e| format!("cannot listen on {}: {}", want, e))?;
            let a = l.local_addr()?;
            (Some(l), Some(a))
        }
        None => (None, None),
    };
    let (state, generation_rx) = Shared::new(rpc, policy, equihash, config.password.clone(), config.limits.clone());
    Ok(Bound { addr, status_addr, listener, status_listener, state, generation_rx })
}

/// Runs the pool until the process is stopped.
pub async fn run(rpc: RpcClient, config: Config) -> Result<(), Box<dyn std::error::Error>> {
    bind(rpc, config).await?.serve().await;
    Ok(())
}
