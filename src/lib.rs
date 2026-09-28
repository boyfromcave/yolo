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
use state::Shared;
use work::Policy;

/// Everything `run` needs, as the CLI resolves it.
#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub status_bind: Option<SocketAddr>,
    pub policy: Policy,
    pub password: Option<String>,
    pub cenote: u32,
    pub equihash: EquihashArg,
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
            "yolo {} mode {} listening on {} (Equihash {}, node {})",
            env!("CARGO_PKG_VERSION"),
            self.state.policy.mode,
            self.addr,
            self.state.equihash,
            self.state.rpc.url()
        );
        tokio::spawn(poller::run(self.state.clone()));
        if let Some(l) = self.status_listener {
            info!("status on http://{}/status", self.status_addr.unwrap());
            tokio::spawn(status::serve(self.state.clone(), l));
        }
        stratum::serve(self.state, self.listener, self.generation_rx).await;
    }
}

/// Resolves the Equihash parameters against the node and binds the listeners.
pub async fn bind(rpc: RpcClient, config: Config) -> Result<Bound, Box<dyn std::error::Error>> {
    let equihash = resolve_equihash(&rpc, config.equihash).await;
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
    let (state, generation_rx) = Shared::new(rpc, config.policy.clone(), equihash, config.password.clone(), config.cenote);
    Ok(Bound { addr, status_addr, listener, status_listener, state, generation_rx })
}

/// Runs the pool until the process is stopped.
pub async fn run(rpc: RpcClient, config: Config) -> Result<(), Box<dyn std::error::Error>> {
    bind(rpc, config).await?.serve().await;
    Ok(())
}
