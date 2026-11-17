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
pub mod pace;
pub mod pairs;
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
use rpc::{CoinbaseSource, RpcClient};
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
    /// Test only: where paired coinbases come from instead of the node's `getcoinbasetemplatefor`
    /// (the regtest stub until a node serves the call, and the in-process fakes). None in every
    /// production invocation: the CLI never sets it.
    pub test_coinbase_source: Option<std::sync::Arc<dyn CoinbaseSource>>,
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

/// Where paired coinbases come from, and whether the call is there now: none under `--payout`
/// (every job is a swap job), the test source when one is given, else the node. While the call is
/// not there, Sapling and post-quantum usernames are refused at `mining.authorize` and the node is
/// asked again every `--coinbase-probe` period; a "Method not found" answer to any request later
/// switches paired jobs off the same way.
async fn resolve_coinbase_source(rpc: &RpcClient, policy: &Policy, test: Option<std::sync::Arc<dyn CoinbaseSource>>) -> (Option<std::sync::Arc<dyn CoinbaseSource>>, bool) {
    if policy.payout.is_some() {
        return (None, false);
    }
    if let Some(t) = test {
        tracing::warn!("paired coinbases come from a TEST source, not the node's getcoinbasetemplatefor");
        return (Some(t), true);
    }
    let r = rpc.clone();
    let source: std::sync::Arc<dyn CoinbaseSource> = std::sync::Arc::new(rpc.clone());
    match tokio::task::spawn_blocking(move || r.has_coinbase_call()).await {
        Ok(Ok(true)) => {
            info!("the node serves getcoinbasetemplatefor: Sapling and post-quantum usernames are paid by paired jobs");
            (Some(source), true)
        }
        // `Pairs::set_available` says so once
        Ok(Ok(false)) => (Some(source), false),
        Ok(Err(e)) => {
            // The node answered getblockchaininfo a moment ago; assume the call exists. A
            // "Method not found" answer to the first request switches paired jobs off.
            tracing::warn!("cannot tell whether the node serves getcoinbasetemplatefor ({}); assuming it does", e);
            (Some(source), true)
        }
        Err(e) => {
            tracing::error!("getcoinbasetemplatefor probe task: {}", e);
            (Some(source), false)
        }
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

    /// Serves until the task is dropped, or until something makes the pool stop itself
    /// (`Shared::fail`: the node came back on a chain block pacing is refused on), which is the
    /// error returned.
    pub async fn serve(self) -> Result<(), String> {
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
        let mut tasks = AbortOnDrop(vec![tokio::spawn(poller::run(self.state.clone())), tokio::spawn(pairs::run(self.state.clone()))]);
        let pace = self.state.limits.min_block_interval;
        if !pace.is_zero() {
            let window = self.state.lock().pacer.selection_window();
            info!(
                "block pacing: a block is submitted no sooner than {:?} after its job's time was read from the node; \
                 the held block is one at random among the solutions of a tip's first {:?}",
                pace, window
            );
            tasks.0.push(tokio::spawn(pace::run(self.state.clone())));
        }
        if let Some(l) = self.status_listener {
            info!("status on http://{}/status", self.status_addr.unwrap());
            tasks.0.push(tokio::spawn(status::serve(self.state.clone(), l)));
        }
        let mut fatal = self.state.fatal.subscribe();
        let result = tokio::select! {
            _ = stratum::serve(self.state.clone(), self.listener, self.generation_rx) => Ok(()),
            why = fatal.wait_for(|f| f.is_some()) => Err(why.ok().and_then(|f| f.clone()).unwrap_or_else(|| "stopped".into())),
        };
        drop(tasks);
        result
    }
}

fn policy_payout_is_username(payout: &Option<Payout>) -> bool {
    payout.is_none()
}

/// The node's chain name (`getblockchaininfo` `chain`), retrying while it is unreachable. An
/// answer without the field counts as mainnet, the careful reading.
pub async fn resolve_chain(rpc: &RpcClient) -> String {
    loop {
        let r = rpc.clone();
        match tokio::task::spawn_blocking(move || r.getblockchaininfo()).await {
            Ok(Ok(info)) => return info.get("chain").and_then(|c| c.as_str()).unwrap_or("main").to_string(),
            Ok(Err(e)) => tracing::warn!("getblockchaininfo: {} (retrying in 5 s)", e),
            Err(e) => tracing::error!("getblockchaininfo task: {}", e),
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// Resolves the Equihash parameters against the node and binds the listeners.
pub async fn bind(rpc: RpcClient, config: Config) -> Result<Bound, Box<dyn std::error::Error>> {
    if !config.limits.min_block_interval.is_zero() {
        // Refused on mainnet before anything listens: a block held there is a block lost.
        pace::check_chain(&resolve_chain(&rpc).await, config.limits.min_block_interval)?;
    }
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
    let (coinbase, call_available) = resolve_coinbase_source(&rpc, &policy, config.test_coinbase_source.clone()).await;
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
    let (state, generation_rx) = Shared::new(rpc, policy, coinbase, equihash, config.password.clone(), config.limits.clone());
    if state.coinbase.is_some() {
        state.pairs.set_available(call_available);
    }
    Ok(Bound { addr, status_addr, listener, status_listener, state, generation_rx })
}

/// Runs the pool until the process is stopped.
pub async fn run(rpc: RpcClient, config: Config) -> Result<(), Box<dyn std::error::Error>> {
    bind(rpc, config).await?.serve().await?;
    Ok(())
}
