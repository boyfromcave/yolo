// Copyright (c) 2026 The Ycash developers
// Distributed under the MIT software license, see the accompanying
// file LICENSE or https://www.opensource.org/licenses/mit-license.php .

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

use yolo::equihash::EquihashArg;
use yolo::rpc::{read_cookie, ConfFile, RpcAuth, RpcClient};
use yolo::Config;

/// Ycash stratum pool (yolo), Yellowback tag aware.
///
/// Two flags decide the coinbase. --payout unset: each miner's stratum username is its
/// payout address (checked with validateaddress); set: every block pays that address and
/// the username is just a worker name. --text unset: the node's coinbase scriptSig is used
/// as is; set: it is rebuilt as height push, coinbaseaux.flags (the Yellowback tag) and the
/// text, within 100 bytes. The tag is carried in every combination.
#[derive(Parser, Debug)]
#[command(name = "yolo", version, about, long_about = None)]
struct Cli {
    /// Pay every block to this transparent address (unset: the miner's username is paid).
    #[arg(long, value_name = "ADDRESS")]
    payout: Option<String>,
    /// Rebuild the coinbase scriptSig with this text after the node's coinbaseaux.flags
    /// (unset: the node's scriptSig untouched). Truncated to fit 100 bytes, with a warning.
    #[arg(long, value_name = "TEXT")]
    text: Option<String>,
    /// Stratum listen port.
    #[arg(long, default_value_t = 3333)]
    port: u16,
    /// Address to bind the stratum listener to.
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,
    /// Password miners must send in mining.authorize (unset: any).
    #[arg(long)]
    password: Option<String>,
    /// Node JSON-RPC URL (default from --conf, else http://127.0.0.1:18232 on regtest / 8832).
    #[arg(long, value_name = "URL")]
    rpc: Option<String>,
    /// RPC username (with --rpc-password).
    #[arg(long, value_name = "USER")]
    rpc_user: Option<String>,
    /// RPC password (with --rpc-user).
    #[arg(long, value_name = "PASSWORD")]
    rpc_password: Option<String>,
    /// Read the RPC credentials from the node's .cookie file.
    #[arg(long, value_name = "PATH")]
    rpc_cookie: Option<PathBuf>,
    /// Read rpcuser/rpcpassword/rpcport/regtest from a ycash.conf.
    #[arg(long, value_name = "ycash.conf")]
    conf: Option<PathBuf>,
    /// Equihash parameters: auto (regtest → 48,5, else 192,7), 48,5 or 192,7.
    #[arg(long, default_value = "auto")]
    equihash: EquihashArg,
    /// Serve GET /status (JSON) on this port.
    #[arg(long)]
    status_port: Option<u16>,
    /// Log level: error, warn, info, debug, trace.
    #[arg(long, default_value = "info")]
    log: String,
    /// Test only: --text without the coinbaseaux.flags append (reproduces the Perl cenote; drops the tag).
    #[arg(long, hide = true)]
    no_flags: bool,
}

fn main() {
    let cli = Cli::parse();
    let level: tracing::Level = cli.log.parse().unwrap_or_else(|_| {
        eprintln!("--log: expected error|warn|info|debug|trace, got {:?}", cli.log);
        std::process::exit(2);
    });
    tracing_subscriber::fmt().with_max_level(level).with_target(false).init();
    if cli.no_flags && cli.text.is_none() {
        fail("--no-flags is meaningful only with --text");
    }

    let conf = cli.conf.as_deref().map(|p| ConfFile::load(p).unwrap_or_else(|e| fail(&e)));
    let url = cli
        .rpc
        .clone()
        .or_else(|| conf.as_ref().map(ConfFile::url))
        .unwrap_or_else(|| "http://127.0.0.1:18232".to_string());
    let auth = if let Some(cookie) = &cli.rpc_cookie {
        read_cookie(cookie).unwrap_or_else(|e| fail(&e))
    } else if let (Some(user), Some(password)) = (cli.rpc_user.clone(), cli.rpc_password.clone()) {
        RpcAuth { user, password }
    } else if let Some(c) = conf.as_ref().filter(|c| c.rpcuser.is_some() && c.rpcpassword.is_some()) {
        RpcAuth { user: c.rpcuser.clone().unwrap(), password: c.rpcpassword.clone().unwrap() }
    } else if let Some(cookie) = conf.as_ref().and_then(|c| c.datadir.clone()).map(|d| {
        let mut d = d;
        if let Some(c) = conf.as_ref() {
            if c.regtest {
                d.push("regtest")
            } else if c.testnet {
                d.push("testnet3")
            }
        }
        d.join(".cookie")
    }) {
        read_cookie(&cookie).unwrap_or_else(|e| fail(&format!("no RPC credentials: {}", e)))
    } else {
        fail("no RPC credentials: give --rpc-user/--rpc-password, --rpc-cookie or --conf")
    };
    let rpc = RpcClient::new(&url, &auth);

    let bind: SocketAddr = format!("{}:{}", cli.bind, cli.port).parse().unwrap_or_else(|e| fail(&format!("--bind: {}", e)));
    let status_bind = cli.status_port.map(|p| SocketAddr::new(bind.ip(), p));
    let config = Config {
        bind,
        status_bind,
        payout: cli.payout.filter(|a| !a.is_empty()),
        text: cli.text.map(String::into_bytes),
        no_flags: cli.no_flags,
        password: cli.password.filter(|p| !p.is_empty()),
        equihash: cli.equihash,
    };

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap_or_else(|e| fail(&e.to_string()));
    let result = runtime.block_on(async {
        tokio::select! {
            r = yolo::run(rpc, config) => r,
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Bye!");
                Ok(())
            }
        }
    });
    if let Err(e) = result {
        fail(&e.to_string());
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("yolo: {}", msg);
    std::process::exit(1);
}
