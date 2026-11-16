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
- **Post-quantum vaults:** the post-quantum layer rides the same activation and leaves the
  coinbase, the quote tag and the mining RPCs as they are, so yolo needs no change for it either.
  Blocks may then carry large post-quantum transactions; yolo passes the node's template through.
- **Miners at their own address, of any kind:** a transparent, Sapling or post-quantum address as
  the stratum username is paid in full by the block it finds (*Which address gets paid*).
- **Node it needs:** any ycashd v4.5.0, or 6.20.0 or later, for transparent miners; ycashd
  6.22.0-beta3 or later (`getcoinbasetemplatefor`) for Sapling and post-quantum miners; for tagged
  blocks, a ycashd built from the `upgrade/vault` branch of
  [ycash-dd](https://github.com/boyfromcave/ycash-dd) or
  [ycash6](https://github.com/boyfromcave/ycash6) (see *Node requirements*).
- **More:** the parent project, [Ycash vaults and Yellowback](https://github.com/boyfromcave/yellowback).
- **Status: proposed, not live on mainnet.** It runs on regtest and on the project's fast testnet
  (testnet-yb-fast), where the vault upgrade is active. The upgrade has not been adopted by the
  Ycash Foundation, has not been audited, and has no activation height on mainnet.
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

Rust 1.85 or newer (`rust-toolchain.toml` pins the release builds to 1.91.0), no OpenSSL (the
HTTP client uses rustls):

```
cargo build --release
./target/release/yolo --help
```

`cargo test` runs the unit tests and the wire-format replay of the recorded Perl exchanges
(`tests/fixtures/`); `cargo test --features regtest` adds the end-to-end test against a real
node (see *Regtest quick start*).

## Which address gets paid

`--payout` decides whose address the block pays:

| `--payout` | Who is paid | The stratum username | Perl original |
|---|---|---|---|
| unset (default) | each miner, at its **stratum username**: a transparent (`s1…`, `s3…`), Sapling (`ys1…`) or post-quantum (`sqf…`, `sqs…`) address, optionally followed by a worker label, `<address>.<label>` | checked with `validateaddress` (and `z_validateaddress` for a Sapling address) on `mining.authorize`; anything else is refused with `Invalid address` | `stratumpool`, `cenote` |
| `--payout <s1…>` | **this address**, every block, whoever found it | any worker name; not checked | `stratumsolo`, `cenote --scrooge` |

The label is 1 to 32 characters of `A-Z a-z 0-9 _ -`, everything after the first `.`; it is shown
in the log and on `/status` only, and never changes who is paid.

**Post-quantum miners: mine to an address from ycashd or yewd.** A block reward paid to a
post-quantum address is a coinbase, and the network lets a coinbase move only to shielded
recipients, with no change. Today only the ycashd wallet (Ycash 6.22.0-beta3 or later, either node
line) and yewd (the wallet beside koganyd, in the [yew](https://github.com/boyfromcave/yew)
repository) can do that: `z_sendmany` from the address to a Sapling address, once the reward has
101 confirmations, sending the exact amount the wallet names when it refuses a send that would
leave change. The YEW phone app and the web wallet cannot yet: they do not know that a coin is a
coinbase, so a reward paid to one of their addresses shows as spendable and their sends are
refused.

**How each miner is paid in full.** A miner who finds a block gets the block subsidy plus every
fee of the transactions in that block. The only thing that can come out of it is the
development-fund share, and only the node applies it: where consensus requires it, or where the
pool node is started with a nonzero `-ydf` (below). `yolo` never chooses, asks for or lowers an
amount: the node derives it from the template. There are two
ways the coinbase is made, by the kind of the username's address:

- **Transparent (P2PKH, P2SH): the swap path.** The node's own coinbase (`coinbasetxn`) already
  claims the exact amount for the template's transactions. `yolo` rewrites the `scriptPubKey` of
  its `vout[0]`, the miner's reward, structurally (a v4 transaction parser, not a regex over the
  hex), and leaves every other output exactly as the node built it.
- **Sapling and post-quantum: paired jobs.** A Sapling payout has no script to swap in (it is a
  Sapling output with a proof and a binding signature, which only the node can build). For the
  template it holds, `yolo` asks the node for that miner's coinbase,
  `getcoinbasetemplatefor <address> {"previousblockhash": …, "txids": […]}`, and issues the
  **pair** (that template's transactions, all of them, and that coinbase) as one job. It checks
  every answer against the stock template before using it: the height and parent; the fee total;
  the coinbase's total equal to the node's own coinbase's; the fund output identical; the
  coinbase's sigops with the template's within its `sigoplimit` (and, when the job is built, the
  block within its size limit); for a post-quantum payout, an output of exactly the amount to the
  miner's script. **For a Sapling payout `yolo` checks the shape and the value, not the
  recipient:** exactly one Sapling output, a `valueBalance` of minus the amount, and no
  transparent output besides the fund's. That the Sapling output is addressed to the miner is the
  node's responsibility: `yolo` does not decrypt it (it could, with the all-zero outgoing viewing
  key of ZIP 213, but that needs a Sapling crate it does not carry). It never pairs a coinbase
  with any transaction set other than the one it was built for. One request serves every
  connection of one address. Under load the address whose pair is the oldest is served first.
  One source IP may hold at most `--max-paired-per-ip` (default 8) Sapling and post-quantum
  payout addresses at once; further ones from it are refused at `mining.authorize`, transparent
  ones are not. The pair is renewed at once
  on a new block (the miner gets no work until its pair arrives, a fraction of a second, a few
  seconds for a Sapling proof on a small host) and on a new Yellowback quote, and on a changed
  transaction set no more often than every 5 seconds (the miner keeps its valid job meanwhile).
  At most `--coinbase-concurrency` requests (default 2) are in flight, on their own threads:
  the template poll and `submitblock` never wait behind them. Give the node `-rpcthreads` of at
  least that plus 2. `--text` rewrites only the scriptSig of a paired coinbase; its outputs are
  never touched. Post-quantum addresses take this path too (decision D-6 of the design note),
  although their payout is a script: only P2PKH and P2SH are swapped.

The design is `docs/plans/beta3/getcoinbasetemplatefor.md` in the workspace.

**The development-fund share is not always there.** Consensus requires a fund output (5 % of the
subsidy) only below the fund's mandate end height (mainnet: ended at 2,275,000; the testnets and
regtest have their own). Past it the share is optional and comes only from the pool node's
`-ydf=<n>` (a percentage of the subsidy, 0 to 100), whose default is **0** since beta3: the
coinbase then has one output and the miner gets the whole subsidy plus fees. Run a pool node at
the default. A node started with `-ydf=5` adds the share to its own coinbase and to every coinbase
`getcoinbasetemplatefor` builds, so both paths pay a miner the same, and that node donates 5 % of
every miner's subsidy: an operator who sets it must tell the miners.

The address given to `--payout` is validated once at startup (`validateaddress`; a shielded or
invalid address is a startup error). There is no "leave the coinbase as the node built it"
case any more: the old solo behaviour is exactly `--payout <the node's own mineraddress>`,
and the block it produces is byte for byte the node's. Under `--payout` every job is a swap job.

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
 "nodeUp":true,"height":104,"templateAgeSeconds":0.0,"miners":2,
 "minersByKind":{"p2pkh":1,"p2sh":0,"pq":0,"sapling":1,"fixed":0},
 "minerList":[{"id":0,"kind":"p2pkh","address":"s1…","label":null},{"id":1,"kind":"sapling","address":"ys1…","label":"rig1"}],
 "coinbaseCall":true,"pairs":{"addresses":1,"waiting":0,"inFlight":0,"concurrency":2,"requests":3,"failures":0,"lastRoundTripMs":180},
 "connections":2,
 "tag":"quote","lastSubmitVerdict":"accepted","accepted":1,"rejected":0,"stale":0,"highHash":0,"uptimeSeconds":12}
```

`payout` is the string `"username"` or the fixed `--payout` address; `text` is whether
`--text` is set. `miners` counts authorized miners, `connections` every open stratum socket.
`minersByKind` and `minerList` show each authorized miner by payout kind (`p2pkh`, `p2sh`, `pq`,
`sapling`, or `fixed` under `--payout`, where the label is the username); `coinbaseCall` says
whether paired jobs are available; `pairs` counts the paired addresses, those `waiting` for a
coinbase for the current tip, the requests in flight and the last round trip.
`lastSubmitVerdict` is the string `"accepted"` on success, the exact string `submitblock`
returned (`duplicate`, `high-hash`, `time-too-old`, …), or one of the pool's own verdicts for
a submit that never reached the node: `bad-submit` (malformed), `bad-ntime` (an `ntime` other
than the job's), `duplicate` (a header this connection already sent), `high-hash` (the block hash
is above the target — checked locally before `submitblock`), `bad-solution` (meets the block
target with an Equihash solution that does not verify), `stale` (see below), `job-full`
(a share on a job that already holds 128);
`tag` is the kind found in the last coinbase built. An accepted block also triggers an immediate `getblocktemplate`
(not the next 1 s poll), and no job is handed out on the old parent in between: a fast solver
would only re-solve it and be rejected `inconclusive`. To verify a mined block on the node,
`ycash-cli yed_gettag <height>` and the operator kit's `check-coinbase <height>`
(`contrib/yellowback/pool/`) decode the stored block the same way and must agree with the
pool's log line.

Every request gets an answer. A refused submit is answered `"result": false` with the stratum
error triple the miners read, `[code, message, null]`: 21 for a stale submit, 22 `duplicate`,
23 `high-hash`, 20 with the verdict for anything else (`bad-ntime`, `bad-solution`, the node's
verdict, …); a submit before `mining.authorize` gets 24 and an unknown method 20, both before the
socket is closed. A submit is **stale** when its job id is not one of the connection's last eight
jobs (`Job not found`) or its job is on a parent that is no longer the tip (`Stale share`: a
newer block came in, or this pool's own block was just accepted). A stale submit is answered at
once, never forwarded (its block could only be a fork at a height the chain already has), never
counted against the miner, counted in `stale` (not in `rejected`), and leaves the miner on the
job it holds: a GPU miner still submits the solutions it found for the previous job after a
clean `mining.notify` (miniZ did, on testnet-yb-fast), and each must get its answer rather than
a new job. The share-level refusals leave the miner on its job too (`high-hash`, the pool's
`duplicate`, `job-full`): they hand out no new job, which would evict the kept jobs a GPU is still solving and turn
its next solutions into `Job not found`. A block (accepted or not) and a malformed or invalid
submit (`bad-submit`, `bad-ntime`, `bad-solution`) still start the miner over on fresh work.
A `high-hash` submit is counted in `highHash`, like a stale one neither in `rejected` nor
against the miner: miniZ sends about one per five or six shares, and in a long block gap they
would add up to a disconnect. It costs the pool one SHA256d, the same as a share, so a flood of
them is bounded the way shares are (the connection caps and the line limit). `duplicate` and
`job-full` are counted in `rejected`; `duplicate`, a malformed submit and `bad-solution` (a block
hash with an Equihash solution that does not verify) count against the miner.
Kept jobs on the tip's parent stay valid: a job replaced at the same height (new transactions, a
new `curtime`) still builds a block on the tip and is still forwarded. A block the node refuses
because it lost a race (`inconclusive`, `duplicate`, `duplicate-inconclusive`, `bad-prevblk`)
is answered with code 21 and its verdict.

## Flags

```
yolo [OPTIONS]

  --payout <ADDRESS>         pay every block to this transparent address (unset: the miner's username is paid,
                             transparent, Sapling or post-quantum, optionally <address>.<label>)
  --text <TEXT>              rebuild the coinbase scriptSig with this text after the node's coinbaseaux.flags
                             (unset: the node's scriptSig untouched); truncated to fit 100 bytes, with a warning
  --port <PORT>              stratum listen port                          [default: 3333]
  --bind <BIND>              address to bind the stratum listener to     [default: 0.0.0.0]
  --password <PASSWORD>      password miners must send in mining.authorize (unset: any)
  --rpc <URL>                node JSON-RPC URL (default from --conf, else http://127.0.0.1:18832 on regtest / 8832)
  --rpc-user <USER>          RPC username (with --rpc-password)
  --rpc-password <PASSWORD>  RPC password (with --rpc-user)
  --rpc-cookie <PATH>        read the RPC credentials from the node's .cookie file
  --conf <ycash.conf>        read rpcuser/rpcpassword/rpcport/regtest from a ycash.conf
  --equihash <EQUIHASH>      auto (regtest → 48,5, else 192,7), 48,5 or 192,7 [default: auto]
  --status-port <PORT>       serve GET /status (JSON) on this port
  --status-bind <BIND>       address to bind the status listener to       [default: 127.0.0.1]
  --share-ratio <N>          the target sent to miners is N times easier than the block's; a submit
                             between the two is a share (counted, never forwarded)      [default: 1]
  --coinbase-concurrency <N> getcoinbasetemplatefor requests in flight at once (paired jobs) [default: 2]
  --max-paired-per-ip <N>    Sapling and post-quantum payout addresses one IP may mine to at once [default: 8]
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
username = its payout address, transparent, Sapling or post-quantum, optionally with a worker
label (`<address>.<label>`), or any worker name when `--payout` is set; password =
`--password` or anything. `mining.extranonce.subscribe` is acknowledged; job ids are a
per-connection counter.

### Shares (`--share-ratio`)

yolo is a solo pool: by default the target it sends (`mining.set_target`) is the block's, so a
miner submits only blocks. Once a network's difficulty has caught up with a GPU fleet, each card
finds a block only every few minutes, and an authorized miner that says nothing for three minutes
is disconnected (*Exposure*). `--share-ratio N` sends a target N times easier than the block's
(at most the all-ones target): a submit that meets it but not the block target is a **share**,
answered `true`, counted in `/status` (`shares`) and never sent to the node; one that meets the
block target goes to `submitblock` once its Equihash solution verifies. The miner keeps its job
after a share. A share already counted on any of the connection's last eight jobs is refused
`duplicate`; a job remembers at most 128 headers (32 bytes each, so at most 32 KiB per
connection and 32 MiB at the default 1,024 connections, about twice that with the set's overhead): the share that fills it moves the miner to a fresh job, and a later share on the
full job is refused `job-full` without counting against the miner. Shares pay
nothing (the coinbase pays `--payout` or the username whatever the shares say) and their Equihash
solution is not checked, only their hash against the share target: they show that a miner is
alive and, as `shares × N` solutions-worth per block interval, roughly how fast the fleet is.
Pick N so a card submits every few seconds to a minute: about (block interval in seconds × cards
on the pool) / (seconds between a card's shares), e.g. 64 for a few cards on 75-second blocks.

### Exposure

Stratum is plaintext TCP. `--password` is an **access gate, not a secret channel**: every miner
shares it and it crosses the network in clear (the compare on the pool side is
constant-time, which is all that can be done there). A private pool belongs behind a firewall
or an allowlist. The pool itself is bounded against misbehaving peers: a line longer than
8 KiB, a socket that has not authorized within 30 s, a miner silent for three keepalive
periods (3 min), a `mining.authorize` for a second worker (the same worker again is answered
`true` without a node call), more than ten `mining.authorize` a minute from one IP that fail
or need a `validateaddress` call (a successful one answered from the address cache or under
`--payout` is not counted, so a miner reconnecting after a stale share re-authorizes freely), or
ten refused submits since the miner's last accepted block (malformed, an `ntime`
other than the job's, a duplicate, above the target, an invalid Equihash solution, or a block
the node rejected other than for a lost race: `inconclusive`, `duplicate`,
`duplicate-inconclusive`, `bad-prevblk`), less one for every 32 accepted shares, all close the
socket; at most
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
- The RPCs yolo calls: `getblocktemplate`, `submitblock`, `getblockchaininfo` and
  `validateaddress`, plus `z_validateaddress` and `getcoinbasetemplatefor` for Sapling and
  post-quantum usernames. It never calls a wallet-spending or `yed_*` RPC.
- **For Sapling and post-quantum usernames: a ycashd with `getcoinbasetemplatefor`** (Ycash
  6.22.0-beta3 or later, either node line). `yolo` asks the node at startup; a node without it
  still serves transparent miners exactly as before, and Sapling and post-quantum usernames are
  refused with `Invalid address: this pool cannot pay <kind> addresses now (its node does not
  serve getcoinbasetemplatefor)`, logged once. A "Method not found" answer to any later request
  (a startup check that could not reach the node, a downgraded node) does the same and lets the
  paired miners go; `yolo` asks the node again every minute and accepts them again once it
  serves the call. `-rpcthreads` at least `--coinbase-concurrency` plus 2.
- `-ydf=0` (the default since beta3; leave it there) pays miners the whole subsidy past the fund's
  mandate; see *Which address gets paid*.
- **`mineraddress=s1…`**: a transparent address **of the node wallet** (`ycash-cli getnewaddress`),
  never a Sapling one (the swap path needs the node's coinbase to pay a transparent `vout[0]`).
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
- ycashd v4.5.0, or 6.20.0 or later (either node line). The header root is read from `lightclientroothash` (v4.5.0, and
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
ycash-cli -regtest … set_create '{"seats":15,"unlockthreshold":1,"cancelthreshold":1,"slashthreshold":1,"open":true,"maturity":2}'
ycash-cli -regtest … generate 1                              # the result's setid is now on chain
#   (maturity 2: the post-quantum line's regtest floor; upgrade/vault nodes accept 1 or more)
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

`tests/regtest_pairs.rs` (`cargo test --features regtest --test regtest_pairs`) is the paired-job
acceptance test: one pool, a transparent, a Sapling (with a worker label) and two post-quantum
miners, one at a time and then three at once, every block with fee-paying transactions and
checked on chain to pay its finder exactly the subsidy plus every fee; then the pool node at
`-ydf=5`, where each pays 95 % of the subsidy plus every fee and its fund output is the stock
coinbase's. `YOLO_REGTEST_COINBASE=node` uses the node's `getcoinbasetemplatefor` (and fails if
`YCASHD` lacks it); `stub` the test-only stand-in for nodes without it (a helper node mining to the
Sapling address, and the pool node's coinbase with the post-quantum script swapped in); unset,
`node` when the call exists, else `stub`. `tests/paired.rs` drives the same flow in-process against
a fake node and source: refused and stale answers, template changes in the middle of a job, the
request cap.

The nodes start with the vault upgrade active at height 110: yolo first mines blocks 102
through 111 with no Yellowback state (case `across-vault-activation`: every block accepted, the
tip's branch ID `6d5b7a31` on both nodes), then node A creates the YED attestor set
(`set_create`, one block), both nodes restart with `-yellowbackattestorset=<setid>`, and the six
cases run as above. `YOLO_REGTEST_VAULT=<h>` (h ≥ 104) moves the activation height.
`YOLO_REGTEST_KOGANY=<koganyd>` (a kogany node built with `--features ycash`, the third node
line) runs the same cases with yolo serving kogany's templates: kogany joins as a third node
peered with node A, the two ycashd nodes keep the wallet roles and check every block (kogany never
solves Equihash), and until kogany registers the YED module its tag carries its mock's payout key.

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
- The job time is the template's `curtime`, not the wall clock: the node keeps it within
  median-time-past + 1 and median-time-past + 3 hours, so a chain whose median-time-past runs
  ahead of the clock (a burst-mined regtest chain) or far behind it (a testnet left idle for more
  than three hours) still gets a valid header time.
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
