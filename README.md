# yolo — Ycash stratum pool

`yolo` is a small stratum server that connects Equihash GPU miners to a Ycash full node as a
**solo pool**: miners only get paid when they find a block, and the payment is the block's own
coinbase. It is a Rust rewrite of `yecdev/yolo` (ChileBob's Perl `stratumpool`, `stratumsolo`
and `cenote`, now under [`legacy/perl/`](legacy/perl/README.md)), **wire-compatible** with the
miners the Perl served — gminer, miniZ and lolMiner at Equihash 192/7 — and aware of the
**Ycash Yellowback (YED)** coinbase tag: the node's tag reaches the mined block whatever the
flags, including when the coinbase scriptSig is rebuilt.

## Yolo and Ycash Yellowback (YED)

This branch (`upgrade/vault`) mines for nodes that carry a proposed Ycash network upgrade, **the
vault upgrade**, which adds **vaults** to Ycash: YEC locked on chain under rules every node
enforces. **Ycash Yellowback (YED)** is a dollar token built on vaults (`1 YED = 1 US dollar`).
Its YEC/USD price comes partly from mining pools: a pool may put a **quote tag** (its price) in
each block's coinbase, and pools that do earn a share of the pool fee paid on each mint.

- **What yolo does with it:** passes the node's quote tag through to the mined block, whatever
  `--payout` and `--text` say. Every upgraded node enforces YED's rules on every block, so a pool
  has nothing to switch on. yolo needs no change for the upgrade and keeps mining across its
  activation block (tested on regtest). It does not touch the wYEC bridge.
- **Node it needs:** any ycashd v4.5.0 or 6.20.0 for plain mining; for tagged blocks, a ycashd
  built from the `upgrade/vault` branch of [ycash-dd](https://github.com/boyfromcave/ycash-dd) or
  [ycash6](https://github.com/boyfromcave/ycash6) (see *Node requirements*).
- **Status: proposed, not live.** It runs on a local test network (regtest) only. It has not been
  adopted by the Ycash Foundation, has not been audited, and has no activation height on mainnet
  or testnet.
- **Try it:** *Regtest quick start* below, or the node's
  [devnet](https://github.com/boyfromcave/ycash-dd/blob/upgrade/vault/contrib/yellowback/devnet/README.md).

One binary, no modes: two flags, `--payout` and `--text`, decide the coinbase. JSON-RPC
straight to the node (no `ycash-cli` shell-outs), one tokio task per miner plus a template
poller.

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

## Which address gets paid

The block reward is the coinbase's `vout[0]`; `yolo` always rewrites that output's
`scriptPubKey` structurally (a v4 transaction parser, not a regex over the hex) and leaves every
other output — the founders/YDF share — exactly as the node built it. `--payout` decides whose
script goes in:

| `--payout` | Who is paid | The stratum username | Perl original |
|---|---|---|---|
| unset (default) | each miner, at its **stratum username**, which must be a transparent (`s1…`) address | checked with `validateaddress` on `mining.authorize`; anything else is refused with `Invalid address` | `stratumpool`, `cenote` |
| `--payout <s1…>` | **this address**, every block, whoever found it | any worker name; not checked | `stratumsolo`, `cenote --scrooge` |

The address given to `--payout` is validated once at startup (`validateaddress`; a shielded or
invalid address is a startup error). There is no "leave the coinbase as the node built it"
case any more: the old solo behaviour is exactly `--payout <the node's own mineraddress>`,
and the block it produces is byte for byte the node's.

The node's **`mineraddress=` is still required** in both cases: without it there is no
template to serve (the node builds the coinbase, `yolo` only edits it), and the Yellowback
tag's payout key defaults to it (`-yellowbackpayoutaddress=` overrides).

**The tag's `payoutKey` is always the node's, whatever `--payout` says.** The Yellowback tag
inside the coinbase scriptSig comes from the node (`coinbaseaux.flags`) and carries the
node's `-yellowbackpayoutaddress`; `yolo` rewrites only `vout[0]`. So without
`--payout` a miner is paid at its username, but the quote and the pool-fee
eligibility that Yellowback credits to the tag's `payoutKey` accrue to the pool
operator's key, and the miner is **not** a registered Yellowback miner by mining here. `yolo`
logs this once at startup and `/status` says `"tagPayoutKey":"node"`.

## Coinbase text and the Yellowback tag

Under Yellowback a miner's block carries a 36-byte tag in its coinbase scriptSig (the push
`0x24 'Y' 'E' 'D' '!'` followed by version, flags, the YEC/USD price in micro-USD, a source
mask and the miner's payout key) — the miner's price quote; a node without a fresh quote serves
no tag. The node offers the tag to pool software through `getblocktemplate` twice: inside `coinbasetxn.data` (the whole
coinbase the node built, tag included) and as `coinbaseaux.flags` (the tag bytes alone, for
software that assembles its own scriptSig). `--text` decides which carrier `yolo` uses:

| `--text` | The coinbase scriptSig | Perl original |
|---|---|---|
| unset (default) | the node's, untouched: `height push ‖ OP_0 ‖ tag` | `stratumsolo`, `stratumpool` |
| `--text "…"` | rebuilt as `height push ‖ coinbaseaux.flags (verbatim) ‖ push(text)` | `cenote` |

The rebuilt scriptSig is kept within the consensus limit of 100 bytes: the tag is 37 bytes
with its push and the height push up to 5, so the text is truncated to what fits (62 bytes on
mainnet heights) and a warning is logged when that happens. The Perl `cenote` sliced the
height push off and appended its text, discarding the node's tag; the Rust one keeps it.
An empty `--text ""` gives `height push ‖ flags` with nothing pushed after.

A hidden `--no-flags` (meaningful only with `--text`) reproduces the Perl `cenote`'s
tag-dropping scriptSig for the negative test. It only works in a binary built with
`--features regtest`; a release build refuses it at startup, and when it is on, `/status`
reports `"noFlags":true` and every block is logged `tag: none`.

On every template change the server decodes the scriptSig it is about to serve with the node's
own byte scan and logs `tag: quote|signal|none` (`signal` is a tag with a zero price); the same fields are on `GET /status`
(`--status-port`):

```
{"payout":"username","tagPayoutKey":"node","text":true,"noFlags":false,"equihash":"48,5",
 "nodeUp":true,"height":104,"templateAgeSeconds":0.0,"miners":1,"connections":1,
 "tag":"quote","lastSubmitVerdict":"accepted","accepted":1,"rejected":0,"uptimeSeconds":12}
```

`payout` is the string `"username"` or the fixed `--payout` address; `text` is whether
`--text` is set. `miners` counts authorized miners, `connections` every open stratum socket.
`lastSubmitVerdict` is the string `"accepted"` on success, the exact string `submitblock`
returned (`duplicate`, `high-hash`, `time-too-old`, …), or one of the pool's own verdicts for
a submit that never reached the node: `bad-submit` (malformed), `high-hash` (the block hash
is above the target — checked locally before `submitblock`), `stale` (unknown job id);
`tag` is the kind found in the last coinbase built. An accepted block also triggers an immediate `getblocktemplate`
(not the next 1 s poll), and no job is handed out on the old parent in between: a fast solver
would only re-solve it and be rejected `inconclusive`. To verify a mined block on the node,
`ycash-cli yed_gettag <height>` and the operator kit's `check-coinbase <height>`
(`contrib/yellowback/pool/`) decode the stored block the same way and must agree with the
pool's log line.

## Flags

```
yolo [OPTIONS]

  --payout <ADDRESS>         pay every block to this transparent address (unset: the miner's username is paid)
  --text <TEXT>              rebuild the coinbase scriptSig with this text after the node's coinbaseaux.flags
                             (unset: the node's scriptSig untouched); truncated to fit 100 bytes, with a warning
  --port <PORT>              stratum listen port                          [default: 3333]
  --bind <BIND>              address to bind the stratum listener to     [default: 0.0.0.0]
  --password <PASSWORD>      password miners must send in mining.authorize (unset: any)
  --rpc <URL>                node JSON-RPC URL (default from --conf, else http://127.0.0.1:18232 on regtest / 8832)
  --rpc-user <USER>          RPC username (with --rpc-password)
  --rpc-password <PASSWORD>  RPC password (with --rpc-user)
  --rpc-cookie <PATH>        read the RPC credentials from the node's .cookie file
  --conf <ycash.conf>        read rpcuser/rpcpassword/rpcport/regtest from a ycash.conf
  --equihash <EQUIHASH>      auto (regtest → 48,5, else 192,7), 48,5 or 192,7 [default: auto]
  --status-port <PORT>       serve GET /status (JSON) on this port
  --status-bind <BIND>       address to bind the status listener to       [default: 127.0.0.1]
  --max-connections <N>      stratum sockets open at once, authorized or not [default: 1024]
  --max-per-ip <N>           stratum sockets open at once from one IP     [default: 64]
  --log <LOG>                error, warn, info, debug, trace              [default: info]
  -h, --help / -V, --version
```

RPC credentials are resolved in this order: `--rpc-cookie`, `--rpc-user`/`--rpc-password`,
the `rpcuser`/`rpcpassword` of `--conf`, the `.cookie` under the conf's `datadir`. A typical
mainnet start is `yolo --conf ~/.ycash/ycash.conf` (miners are paid at their username) or
`yolo --conf ~/.ycash/ycash.conf --payout s1…` (one address for the whole pool).

A miner connects as it would to any Equihash stratum pool: `stratum+tcp://host:3333`,
username = a transparent address (or any worker name when `--payout` is set), password =
`--password` or anything. `mining.extranonce.subscribe` is acknowledged; job ids are a
per-connection counter.

### Exposure

Stratum is plaintext TCP. `--password` is an **access gate, not a secret channel**: every miner
shares it and it crosses the network in clear (the compare on the pool side is
constant-time, which is all that can be done there). A private pool belongs behind a firewall
or an allowlist. The pool itself is bounded against misbehaving peers: a line longer than
8 KiB, a socket that has not authorized within 30 s, a miner silent for three keepalive
periods (3 min), a second `mining.authorize`, more than ten `mining.authorize` a minute from
one IP, or ten submits in a row that fail the pool's own checks all close the socket; at most
`--max-connections` sockets (and `--max-per-ip` per address) are open at once, at most eight
`submitblock` calls are in flight pool-wide, and every submit's block hash is compared with
the target before it is forwarded, so a flood of random submits never reaches the node.
`validateaddress` answers are cached for ten minutes. The template poll runs on its own
thread, never behind queued submits.

`GET /status` is unauthenticated and bound to loopback by default (`--status-bind`); it
reveals the payout address, height, miner count and the last verdict — bind it elsewhere only
on purpose.

The node's RPC credentials travel as HTTP Basic auth: keep `--rpc` on loopback (or behind a
TLS proxy with `https://`); `yolo` warns at startup otherwise. `--rpc-password` on the command
line is visible to every local user in `ps`; prefer `--rpc-cookie` or `--conf`. From a conf
file, `yolo` connects to `rpcconnect=` (default `127.0.0.1`), never to `rpcbind=`, which is a
listen address.

## Node requirements

- `server=1` and RPC credentials in `ycash.conf`; the node fully synced with at least one
  peer (`getblocktemplate` refuses to serve work otherwise).
- **`mineraddress=s1…`**: a transparent address **of the node wallet** (`ycash-cli getnewaddress`).
  Always needed — without it the node builds no template — and the tag's payout key defaults
  to it. It receives the rewards only under `--payout <that address>`. A mined reward matures
  after 100 blocks and, on Ycash, must first be sent in full to a shielded address before it
  can be spent freely.
- For tagged blocks: a node built from `upgrade/vault` (ycash-dd or ycash6) where YED is live.
  There is no switch: YED is on once the vault upgrade has activated and the network's YED
  attestor set is configured (regtest: `-nuparams=6d5b7a31:<h> -yellowbackattestorset=<setid>`).
  Add the Yellowback payout address (`-yellowbackpayoutaddress=`) and a running quote agent
  (`yed_setquote`). See `doc/yellowback-mining.md` and `contrib/yellowback/pool/README.md` in the
  node repository. Where YED is not live (a stock node, or before the activation) the
  template's `coinbaseaux.flags` is empty and `yolo` behaves exactly as the Perl did (`--text`
  then gives `height push ‖ push(text)`).
- ycashd v4.5.0 or 6.20.0 (either node line). The header root is read from `lightclientroothash` (v4.5.0, and
  6.20.0 while the `gbt_oldhashes` deprecation is allowed, the default) or from 6.20.0's
  `defaultroots`. One combination cannot serve work: a chain before Heartwood (regtest only)
  on 6.20.0 with `-allowdeprecated=none`, whose template carries no Sapling root.
- `-regtest`: `--equihash auto` reads `getblockchaininfo.chain` and switches to 48/5. A GPU
  miner cannot solve 48/5; use the Python `stratum-miner` below.

## Regtest quick start

The Python miner and the recipe live in the node repository
(`contrib/yellowback/devnet/stratum-miner`, `stratum-perl-check`). By hand, on `upgrade/vault`
nodes with the vault upgrade at height 110:

```
# node A: mineraddress= must be a wallet t-addr, so start once to mint it, then restart with it
ycashd -regtest -datadir=$D/a -server -rpcuser=u -rpcpassword=p -rpcport=26301 -port=27301 \
       -listen=1 -discover=0 -keypool=1 \
       -nuparams=5ba81b19:1 -nuparams=76b809bb:1 -nuparams=374d694f:1 -nuparams=8e471bd6:1 \
       -nuparams=66314da3:1 -nuparams=19bd2d2f:1 -nuparams=6d5b7a31:110 \
       -yellowbacksigmaref=0 -mineraddress=$(ycash-cli … getnewaddress)
#   a generated burst runs ahead of the clock: mint the address and generate 101 on the
#   first start with -mocktime=$(( $(date +%s) - 3600 )), then restart without it. (ycashd 6.20.0
#   refuses setmocktime unless started with -mocktime, and there setmocktime 0 means the epoch.)
ycash-cli -regtest … generate 101                            # on the -mocktime start
# node B: the same with -rpcport=26302 -port=27302 -connect=127.0.0.1:27301 (getblocktemplate needs a peer)
# mine past 110 (through yolo, below, or generate); then create the YED attestor set on node A:
ycash-cli -regtest … set_create '{"seats":15,"unlockthreshold":1,"cancelthreshold":1,"slashthreshold":1,"open":true,"maturity":1}'
ycash-cli -regtest … generate 1                              # the result's setid is now on chain
# restart both nodes with -yellowbackattestorset=<setid> added
ycash-cli -regtest … yed_setquote 50000 1                    # $0.05, source 1: the template now carries a quote tag

yolo --rpc http://127.0.0.1:26301 --rpc-user u --rpc-password p --port 26401 --status-port 26402 --log debug \
     --text "my pool"            # optional; add --payout <a t-addr> to pay one address for every block
python contrib/yellowback/devnet/stratum-miner --pool 127.0.0.1:26401 --user <a t-addr> --blocks 1 --equihash 48,5 --verbose
ycash-cli -regtest … yed_gettag <height>
curl http://127.0.0.1:26402/status
```

`cargo test --features regtest` does all of this unattended when `YCASHD` points at the node
binary, ycashd v4.5.0 or 6.20.0 (`STRATUM_MINER`, `PYTHON`, `YOLO_REGTEST_SCRATCH`,
`YOLO_REGTEST_RPC_BASE` and `YOLO_REGTEST_P2P_BASE` override the defaults, which assume the
workspace layout; `YOLO_REGTEST_POOL_NODE_ARGS=-allowdeprecated=none` serves 6.20.0 templates
without the deprecated root keys), one block
per case: the four cells of the payout × text grid (username / `--payout`, with and without
`--text`), `--text --no-flags` (the hidden test switch that reproduces the Perl: the block is
accepted and `yed_gettag` says `found: false`) and a 90-byte `--text` (scriptSig exactly
100 bytes, tag intact). It skips when `YCASHD` is unset.

The nodes start with the vault upgrade active at height 110: yolo first mines blocks 102
through 111 with no Yellowback state (case `across-vault-activation`: every block accepted, the
tip's branch ID `6d5b7a31` on both nodes), then node A creates the YED attestor set
(`set_create`, one block), both nodes restart with `-yellowbackattestorset=<setid>`, and the six
cases run as above. `YOLO_REGTEST_VAULT=<h>` (h ≥ 104) moves the activation height.

## Differences from the Perl

The Perl scripts are the behavioural and wire-format reference (`tests/fixtures/` were recorded
from them and `tests/wire.rs` replays them byte for byte). What changed on purpose:

- **Three scripts became one: why.** The three Perl scripts are one program copy-pasted three
  times — `stratumpool` (2020-10-17); `stratumsolo` two days later, which is pool minus the
  address check and the stats; `cenote` a month later, which is pool plus `--text`, a reward
  burn and `--scrooge`, which is solo again. `cenote --scrooge` ≡ `stratumsolo` and `cenote` ≡
  `stratumpool` + text, so the Rust binary keeps the two things that actually differed as two
  flags (`--payout`, `--text`) and drops the modes, the `--cenote N` burn and `--scrooge`. Job
  ids are the per-connection counter everywhere (`stratumsolo`'s global job is gone) and
  `mining.extranonce.subscribe` is acknowledged without re-issuing the job (`stratumpool`'s
  behaviour; `stratumsolo` re-sent the same job). The wire shapes are unchanged and the
  recorded Perl exchanges still replay.
- `--text` keeps the Yellowback tag: `coinbaseaux.flags` is appended after
  the height push, and the scriptSig is parsed for its real length (the Perl assumed 5 bytes).
- Work is re-issued when `coinbaseaux.flags` changes, not only on height, target or
  sapling-root changes, so a new quote reaches miners within one poll instead of one block.
- `mining.submit` is answered `true` only when `submitblock` returned `null`; every
  rejection string is logged, counted and reported to the miner as `false` (the Perl answered
  `true` to `Block decode failed`, `time-too-old`, …).
- The job time is `max(template.curtime, now)`, not the wall clock alone.
- `nonce1` is exactly 14 bytes / 28 hex chars, as the Perl actually sent (its comment
  said 16).
- The port is a flag (`--port`, default 3333), and the payout output is rewritten
  through a real v4 transaction parser instead of a regex over the hex.
- JSON-RPC over HTTP directly (`--rpc`, `--rpc-cookie`, `--conf`), no `ycash-cli` on `PATH`.
- `--equihash auto`, `GET /status`, the tag log line.

Kept: the stratum message shapes and field order, the byte-reversed header hex, the 60 s
keepalive re-notify, `nTime` taken from the miner's submit, the 2 MB block size cap, the
disconnect on any unknown method.

## Note: nodes on the `harden/yellowback` branch

The nodes' `harden/yellowback` branch carries a version of Yellowback that needs no network
upgrade; yolo mines for it unchanged. There, Yellowback is switched on with
`-experimentalfeatures -yellowback -yellowbackstartheight=1` (drop `-nuparams=6d5b7a31:…` and the
attestor set from the quick start), and `YOLO_REGTEST_LEGACY=1 cargo test --features regtest`
runs the regtest test against such a node. An `upgrade/vault` node refuses
`-yellowbackstartheight` at startup.
