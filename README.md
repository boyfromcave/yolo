# yolo — Ycash solo-pool stratum server

`yolo` is a small stratum server that connects Equihash GPU miners to a Ycash full node as a
**solo pool**: miners only get paid when they find a block, and the payment is the block's own
coinbase. It is a Rust rewrite of `yecdev/yolo` (ChileBob's Perl `stratumpool`, `stratumsolo`
and `cenote`, now under [`legacy/perl/`](legacy/perl/README.md)), **wire-compatible** with the
miners the Perl served — gminer, miniZ and lolMiner at Equihash 192/7 — and aware of the
**Ycash Yellowback (YED)** coinbase tag: every mode carries the node's tag into the mined
block, including the one that rebuilds the coinbase scriptSig.

One binary, three modes, JSON-RPC straight to the node (no `ycash-cli` shell-outs), one tokio
task per miner plus a template poller.

There is no dev fee but don't expect any support!

If you'd like to buy beers for the original developer (@ChileBob):

- YCASH : ys1c8cvazsz5gfp2zhdmzxcarfh4gp6jezdcxnywfcpyuau0l0f9uj99tzvrr6sjw5rfhpsw06lc6n
- ZCASH : zs18zekcwmw9murkl0qazjz5hz4uenaf9pyw53as6f4plx39m92elfyhljgxwc25t3s5uerzqtmf0w

## Build

Stable Rust 1.85 or newer, no OpenSSL (the HTTP client uses rustls):

```
cargo build --release
./target/release/yolo --help
```

`cargo test` runs the unit tests and the wire-format replay of the recorded Perl exchanges
(`tests/fixtures/`); `cargo test --features regtest` adds the end-to-end test against a real
node (see *Regtest quick start*).

## Modes

| Mode | Coinbase | Who is paid | Perl original |
|---|---|---|---|
| `solo` (default) | `coinbasetxn.data` as the node built it | the node's `mineraddress=` | `stratumsolo` |
| `pool` | the payout output (`vout[0]`) rewritten to the miner's address; scriptSig untouched | each miner's stratum username, a transparent (`s1…`) address | `stratumpool` |
| `cenote` | scriptSig rebuilt as height push ‖ `coinbaseaux.flags` ‖ push(`--text`); `vout[0]` rewritten, burned (`--cenote N`) or left to the node (`--scrooge`) | the miner, or nobody, or the node | `cenote` |

In every mode the remaining outputs (the founders/YDF share) stay exactly as the node built
them.

## Flags

```
yolo [OPTIONS]

  --mode <MODE>              solo | pool | cenote                        [default: solo]
  --port <PORT>              stratum listen port (the Perl defaults: 3334 solo/cenote, 3333 pool)
  --bind <BIND>              address to bind the stratum listener to     [default: 0.0.0.0]
  --password <PASSWORD>      password miners must send in mining.authorize (unset: any)
  --text <TEXT>              cenote: text pushed into the scriptSig after coinbaseaux.flags
                                                                        [default: www.FreeSoloMining.com]
  --cenote <N>               cenote: burn the next N block rewards (paid as 0 to the finder) [default: 0]
  --scrooge                  cenote: pay every block to the node's mineraddress regardless of the username
  --rpc <URL>                node JSON-RPC URL (default from --conf, else http://127.0.0.1:18232 on regtest / 8832)
  --rpc-user <USER>          RPC username (with --rpc-password)
  --rpc-password <PASSWORD>  RPC password (with --rpc-user)
  --rpc-cookie <PATH>        read the RPC credentials from the node's .cookie file
  --conf <ycash.conf>        read rpcuser/rpcpassword/rpcport/regtest from a ycash.conf
  --equihash <EQUIHASH>      auto (regtest → 48,5, else 192,7), 48,5 or 192,7 [default: auto]
  --status-port <PORT>       serve GET /status (JSON) on this port
  --log <LOG>                error, warn, info, debug, trace              [default: info]
  -h, --help / -V, --version
```

RPC credentials are resolved in this order: `--rpc-cookie`, `--rpc-user`/`--rpc-password`,
the `rpcuser`/`rpcpassword` of `--conf`, the `.cookie` under the conf's `datadir`. A typical
mainnet start is `yolo --mode pool --conf ~/.ycash/ycash.conf`.

A miner connects as it would to any Equihash stratum pool: `stratum+tcp://host:3334`,
username = anything for `solo`, a transparent address for `pool` and `cenote` (an invalid
address is refused at `mining.authorize`), password = `--password` or anything.

## Node requirements

- `server=1` and RPC credentials in `ycash.conf`; the node fully synced with at least one
  peer (`getblocktemplate` refuses to serve work otherwise).
- **`mineraddress=s1…`**: a transparent address **of the node wallet** (`ycash-cli getnewaddress`).
  Every mode needs it — it is where the node's template pays, and where `solo` and `--scrooge`
  rewards go. A mined reward matures after 100 blocks and, on Ycash, must first be sent in
  full to a shielded address before it can be spent freely.
- For tagged blocks: `-experimentalfeatures -yellowback` plus the Yellowback payout address
  (`-yellowbackpayoutaddress=`) and, for a quote, a running quote agent (`yed_setquote`). See
  `contrib/yellowback/pool/README.md` in the node repository. Without `-yellowback` the
  template's `coinbaseaux.flags` is empty and `yolo` behaves exactly as the Perl did.
- `-regtest`: `--equihash auto` reads `getblockchaininfo.chain` and switches to 48/5. A GPU
  miner cannot solve 48/5; use the Python `stratum-miner` below.

## The Yellowback tag

Under Yellowback a miner's block carries a 36-byte tag in its coinbase scriptSig (the push
`0x24 'Y' 'E' 'D' '!'` followed by version, flags, the YEC/USD price in micro-USD, a source
mask and the miner's payout key) — the miner's price quote, or a bare signal. The node
offers the tag to pool software through **three carriers** of `getblocktemplate`:

1. `coinbasetxn.data` — the whole coinbase, tag included. `solo` submits it as is.
2. `coinbasetxn.data` with the output rewritten — `pool` parses the transaction and replaces
   only `vout[0].scriptPubKey`; the scriptSig, and the tag in it, are untouched.
3. `coinbaseaux.flags` — the tag bytes alone, for software that assembles its own scriptSig.
   `cenote` is that software: the Perl sliced the height push off the node's scriptSig and
   appended its text, discarding everything the node had put there, tag included (Y-F1).
   The Rust `cenote` builds `height push ‖ coinbaseaux.flags (verbatim) ‖ push(text)` and
   keeps the scriptSig within the consensus limit of 100 bytes by truncating the text (the
   tag is 37 bytes with its push, the height push up to 5) — a warning is logged when it does.

On every work build the server decodes the scriptSig it is about to serve with the node's own
byte scan and logs `tag: quote|signal|none`; the same fields are on `GET /status`
(`--status-port`):

```
{"mode":"cenote","equihash":"48,5","nodeUp":true,"height":104,"templateAgeSeconds":0.0,
 "miners":1,"tag":"quote","lastSubmitVerdict":"accepted","accepted":1,"rejected":0,
 "cenoteLeft":0,"uptimeSeconds":12}
```

`lastSubmitVerdict` is `accepted` or the exact string `submitblock` returned (`duplicate`,
`high-hash`, `time-too-old`, …); `tag` is the kind found in the last coinbase built. To verify
a mined block on the node, `ycash-cli yed_gettag <height>` and the operator kit's
`check-coinbase <height>` (`contrib/yellowback/pool/`) decode the stored block the same way and
must agree with the pool's log line.

## Regtest quick start

The Python miner and the recipe live in the node repository
(`contrib/yellowback/devnet/stratum-miner`, `stratum-perl-check`). By hand:

```
# node A: mineraddress= must be a wallet t-addr, so start once to mint it, then restart with it
ycashd -regtest -datadir=$D/a -server -rpcuser=u -rpcpassword=p -rpcport=26301 -port=27301 \
       -listen=1 -discover=0 -keypool=1 \
       -nuparams=5ba81b19:1 -nuparams=76b809bb:1 -nuparams=374d694f:1 -nuparams=8e471bd6:1 \
       -nuparams=66314da3:1 -nuparams=19bd2d2f:1 \
       -experimentalfeatures -yellowback -yellowbackstartheight=1 -yellowbacksigmaref=0 \
       -mineraddress=$(ycash-cli … getnewaddress)
# node B: the same with -rpcport=26302 -port=27302 -connect=127.0.0.1:27301 (getblocktemplate needs a peer)
ycash-cli -regtest … setmocktime $(( $(date +%s) - 3600 ))   # a generated burst runs ahead of the clock (Y-F5)
ycash-cli -regtest … generate 101
ycash-cli -regtest … setmocktime 0
ycash-cli -regtest … yed_setquote 50000 1                    # $0.05, source 1: the template now carries a quote tag

yolo --mode cenote --rpc http://127.0.0.1:26301 --rpc-user u --rpc-password p --port 26401 --status-port 26402 --log debug
python contrib/yellowback/devnet/stratum-miner --pool 127.0.0.1:26401 --user <a t-addr> --blocks 1 --equihash 48,5 --verbose
ycash-cli -regtest … yed_gettag 102
```

`cargo test --features regtest` does all of this unattended when `YCASHD` points at the node
binary (`STRATUM_MINER`, `PYTHON`, `YOLO_REGTEST_SCRATCH`, `YOLO_REGTEST_RPC_BASE` and
`YOLO_REGTEST_P2P_BASE` override the defaults, which assume the workspace layout), one block
per case: `solo`, `pool`, `cenote`, `cenote` without the flags (a hidden test switch that
reproduces the Perl: the block is accepted and `yed_gettag` says `found: false`) and `cenote`
with a 90-byte text (scriptSig exactly 100 bytes, tag intact). It skips when `YCASHD` is unset.

## Differences from the Perl

The Perl scripts are the behavioural and wire-format reference (`tests/fixtures/` were recorded
from them and `tests/wire.rs` replays them byte for byte). What changed on purpose, numbered as
in the workspace plan's findings:

- **Y-F1** `cenote` no longer drops the Yellowback tag: `coinbaseaux.flags` is appended after
  the height push, and the scriptSig is parsed for its real length (the Perl assumed 5 bytes).
- **Y-F2** work is re-issued when `coinbaseaux.flags` changes, not only on height, target or
  sapling-root changes, so a new quote reaches miners within one poll instead of one block.
- **Y-F4** `mining.submit` is answered `true` only when `submitblock` returned `null`; every
  rejection string is logged, counted and reported to the miner as `false` (the Perl answered
  `true` to `Block decode failed`, `time-too-old`, …).
- **Y-F5** the job time is `max(template.curtime, now)`, not the wall clock alone.
- **Y-F6** `nonce1` is exactly 14 bytes / 28 hex chars, as the Perl actually sent (its comment
  said 16).
- **Y-F8** the port is a flag in every mode (`--port`), and `pool`/`cenote` rewrite the coinbase
  through a real v4 transaction parser instead of a regex over the hex.
- JSON-RPC over HTTP directly (`--rpc`, `--rpc-cookie`, `--conf`), no `ycash-cli` on `PATH`.
- `--equihash auto`, `GET /status`, the tag log line.

Kept: the stratum message shapes and field order, the byte-reversed header hex, the 60 s
keepalive re-notify, `nTime` taken from the miner's submit, the 2 MB block size cap, the
disconnect on any unknown method.
