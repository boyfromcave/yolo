use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

use yolo::equihash::EquihashArg;
use yolo::rpc::{read_cookie, ConfFile, RpcAuth, RpcClient};
use yolo::work::{Mode, Policy};
use yolo::Config;

/// Ycash solo-pool stratum server (yolo), Yellowback tag aware.
///
/// Modes: solo pays the node's mineraddress with the coinbase as the node built it; pool
/// pays each miner's stratum username (a transparent address); cenote rebuilds the coinbase
/// scriptSig (height, coinbaseaux.flags, --text) and can burn rewards (--cenote N) or keep
/// them all for the node (--scrooge).
#[derive(Parser, Debug)]
#[command(name = "yolo", version, about, long_about = None)]
struct Cli {
    /// Coinbase policy.
    #[arg(long, value_enum, default_value_t = Mode::Solo)]
    mode: Mode,
    /// Stratum listen port (the Perl defaults: 3334 solo/cenote, 3333 pool).
    #[arg(long)]
    port: Option<u16>,
    /// Address to bind the stratum listener to.
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,
    /// Password miners must send in mining.authorize (unset: any).
    #[arg(long)]
    password: Option<String>,
    /// cenote: text pushed into the coinbase scriptSig after coinbaseaux.flags.
    #[arg(long, default_value = "www.FreeSoloMining.com")]
    text: String,
    /// cenote: burn the next N block rewards (paid as 0 to the finder).
    #[arg(long, default_value_t = 0, value_name = "N")]
    cenote: u32,
    /// cenote: pay every block to the node's mineraddress regardless of the miner's username.
    #[arg(long)]
    scrooge: bool,
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
    /// Test only: cenote without the coinbaseaux.flags append (reproduces the Perl; drops the tag).
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
    if cli.mode != Mode::Cenote && (cli.cenote > 0 || cli.scrooge || cli.no_flags) {
        fail("--cenote, --scrooge and --no-flags apply to --mode cenote only");
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

    let port = cli.port.unwrap_or(match cli.mode {
        Mode::Pool => 3333,
        _ => 3334,
    });
    let bind: SocketAddr = format!("{}:{}", cli.bind, port).parse().unwrap_or_else(|e| fail(&format!("--bind: {}", e)));
    let status_bind = cli.status_port.map(|p| SocketAddr::new(bind.ip(), p));
    let config = Config {
        bind,
        status_bind,
        policy: Policy { mode: cli.mode, text: cli.text.into_bytes(), scrooge: cli.scrooge, no_flags: cli.no_flags },
        password: cli.password.filter(|p| !p.is_empty()),
        cenote: cli.cenote,
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
