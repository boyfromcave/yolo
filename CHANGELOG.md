# Changelog

## Unreleased — beta4

- Tag v2 (rpcversion 11, a post-quantum payout key): yolo's tag reader decodes the 49-byte v2 tag
  (`31 YED! 02`, payout scheme and 32-byte key hash) beside v1, first occurrence deciding, as the
  node does; the log line names the version and the payee scheme, and `/status` gains `tagVersion`
  and `tagPayoutScheme`. The coinbase was never affected: yolo copies the node's flags verbatim, and
  a v2 tag (50 bytes with its push) fits the 100-byte scriptSig with `--text` (truncated to 49 bytes
  at mainnet heights). The node lines' shared vectors (`tests/vectors/yellowback_tag_vectors.json`,
  ycash-dd / ycash6 `src/test/data/`) are checked byte for byte.
- `tests/regtest.rs`: a tag v2 phase on a beta4 node (both post-quantum schemes, plain and 90-byte
  `--text`); `tests/regtest_pairs.rs` phase 4 runs with a post-quantum payout key on a beta4 node, so
  paired coinbases for every kind of miner carry the v2 tag.
- A quote above $100 is read as a quote: yolo's tag reader capped the price at 100,000,000 µUSD,
  the node's ceiling is 4,294,967,295 (`src/yellowback/params.h` `PRICE_MAX`). Only yolo's logs and
  `/status` (`lastTagKind`) were affected; the coinbase always carried the node's flags verbatim.
- `tests/regtest_pairs.rs` phase 4: blocks for transparent, Sapling, FN-DSA and SLH-DSA miners all
  carry the pool node's quote tag; the same quote again makes no new job, a new price exactly one.

## Unreleased — beta3 (the 6.22.0-beta3 release train)

Everything in this section is new since the beta2 train, which pinned yolo `fcb5a9e`.

### What a pool operator must do

- Nothing, for transparent miners or a `--payout` pool: they work as before.
- To pay Sapling and post-quantum miners at their own address: run the pool node on ycashd
  6.22.0-beta3 or later (`getcoinbasetemplatefor`), with `-rpcthreads` of at least
  `--coinbase-concurrency` plus 2.
- Leave the pool node's `-ydf` at its default, 0 (new in the beta3 nodes): a miner is then paid the
  whole subsidy plus every fee past the fund's mandate. Only a node started with a nonzero `-ydf`
  takes a fund share, and its operator must tell the miners.
- GPU miners on a network whose blocks come minutes apart: set `--share-ratio` (below) so they are
  not disconnected as silent.

### Block pacing for testnets (`--min-block-interval`, after the beta3 tags)

- `--min-block-interval <seconds>` (or `YOLO_MIN_BLOCK_INTERVAL`; default 0 = off, at most 300;
  it runs only where the node says `test` or `regtest`, so it is **refused on mainnet** at start,
  and the pool stops if the node comes back on another chain): a block is submitted no sooner than that after its job's time
  was read from the node. Block timestamps end up at least that far apart, so a fast miner on the
  pool no longer drives a test network's difficulty up. The held block is one at random among the
  verified block-target solutions of a tip's first sixth of the interval, always its finder's own
  block, paid in full; the others are answered `true` and credited as shares. A held block is
  dropped, never submitted, when the tip changes under it. With the flag at 0 nothing changes
  except one new `/status` key, `minBlockInterval: 0`. README "Block pacing".
- `/status` with pacing on: `nextBlockInSeconds`, `held`, `heldStale`, `paceCandidates`.

### Paired jobs: Sapling and post-quantum miners paid at their own address

- A stratum username may be a Sapling or a post-quantum address as well as a transparent one, and
  may carry a worker label (`<address>.<label>`, 1 to 32 characters of `A-Z a-z 0-9 _ -`, shown in
  the log and `/status` only). The kind is the node's answer: `validateaddress` (a `pqscheme` means
  post-quantum), then `z_validateaddress` (`sapling`).
- Transparent (P2PKH, P2SH) miners stay on the swap path, unchanged. Sapling and post-quantum
  miners get **paired jobs**: for the template yolo holds, the node builds the miner's coinbase
  (`getcoinbasetemplatefor`, `docs/plans/beta3/getcoinbasetemplatefor.md`), and the job is that
  template's transactions, all of them, with that coinbase. No amount is requested or altered;
  every answer is checked against the stock template (parent, height, transaction count, fee
  total, the coinbase's claim equal to the node's own coinbase's, amount, the fund output identical
  to the stock coinbase's, the coinbase's sigops with the template's within `sigoplimit`, a
  post-quantum payout to the miner's script; for a Sapling payout one Sapling output of the amount
  and no other transparent output than the fund's, the recipient being the node's responsibility)
  and dropped otherwise. Renewed at once on a new
  block or quote, at most every 5 s on a changed transaction set; a miner keeps its valid pair
  meanwhile; a `template-stale` answer fetches a new template. `--coinbase-concurrency` (default 2)
  caps the requests in flight, on threads of their own. `--text` rewrites only a paired coinbase's
  scriptSig. A node without `getcoinbasetemplatefor` serves transparent miners as before and
  Sapling and post-quantum usernames are refused. A "Method not found" answer to any request
  switches paired jobs off the same way (the paired miners are let go, said once in the log) and
  the node is asked again every minute.
- Under load, the address whose pair is oldest is served first (never the address order).
  `--max-paired-per-ip` (default 8) caps the Sapling and post-quantum payout addresses one source
  IP holds at once; over it further ones are refused at `mining.authorize`, transparent ones are not.
- A keepalive never re-sends a paired job whose parent is no longer the tip.
- Templates polled one after another share the transactions they have in common (one allocation
  each), so the template every pair keeps costs no copy of a full block's transactions.
- A paired job never drops transactions at the size limit (its coinbase claims all their fees);
  the swap path refuses a node coinbase that pays a Sapling `-mineraddress`.
- The coinbase codec carries a Sapling coinbase's shielded part as opaque bytes (shape checked).
- A coinbase with no fund output (`-ydf=0`, the node default since beta3, past the fund's mandate)
  is handled and tested (`tests/vectors/regtest-template-111-*.json`).
- `/status`: `minersByKind`, `minerList` (kind, address, label), `coinbaseCall`, `pairs`.
- Tests: `tests/paired.rs` (in-process), `tests/regtest_pairs.rs` (the acceptance test on regtest
  with the Python 48,5 miner; `YOLO_REGTEST_COINBASE=node|stub`).

### Header time after an idle chain, shares, stale submits

- A submit for a job on a parent that is no longer the tip (a newer block came in, or the pool's
  own block was just accepted) or for an unknown job id is **stale**: answered at once with
  `[21, "Stale share" | "Job not found", null]`, never sent to `submitblock`, never counted against
  the miner, counted in `/status` `stale` (no longer in `rejected`), and the miner keeps the job it
  holds. Before, a solution a GPU flushed for the previous job went to `submitblock` as a fork
  block (its answer waited on the node while it was connecting the new tip, and came back
  `inconclusive` with no reason), and every such refusal handed the miner one more clean job. On
  testnet-yb-fast miniZ reconnected after each block, and at a block every 5–10 s its reconnects
  ran into the authorize limit below. Jobs on the tip's parent replaced at the same height are
  still taken.
- A share-level refusal (`high-hash`, the pool's `duplicate`, `job-full`) leaves the miner on its
  job: no `set_target` and clean `mining.notify` follow it. On
  testnet-yb-fast (2026-11-13, `--share-ratio 32`) miniZ sent about one `high-hash` for every
  five shares, and each handed it a fresh job: the connection's eight kept jobs rolled over in a
  few seconds, and solutions miniZ was still finishing for jobs on the tip came back `Job not
  found`. A block and a malformed or invalid submit still start the miner over.
- A `high-hash` submit is counted in the new `/status` field `highHash`, no longer in `rejected`
  nor against the miner (`max_bad_submits`): at miniZ's rate a long block gap would add up to a
  disconnect. It costs one SHA256d, like a share. `bad-solution`, `duplicate` and malformed
  submits still count. The share target is sent and compared as before, checked against a real
  fast-testnet header (block 510,700): big-endian, 64 hex digits, as the node and the Perl give it.
- Every refused request carries the stratum error triple `[code, message, null]` (20 other, 21
  stale, 22 duplicate, 23 high-hash, 24 unauthorized; the Perl sent `"result": false` alone, which
  miniZ logs as "Bad share: unknown"); a submit before `mining.authorize` and an unknown method are
  answered before the socket is closed.
- The per-IP `mining.authorize` budget (ten a minute, audit H-4) counts only attempts that fail
  or need a `validateaddress` call: one answered from the address cache or under `--payout` is
  refunded, so an honest miner reconnecting several times a minute is never refused `Auth
  Failed`, while password guessing and floods of new addresses stay capped. A repeated
  `mining.authorize` for the connection's own worker is answered `true` (no node call); one for
  another worker is answered `Auth Failed` before the socket is closed (it was closed unanswered).

- The job's nTime is the template's `curtime` (the wall clock only when the node sends none),
  no longer `max(curtime, now)`. A chain idle for more than three hours has its median-time-past
  that far behind the clock, the node caps `curtime` at median-time-past + 3 hours
  (`MAX_FUTURE_BLOCK_TIME_MTP`), and every block yolo built on the wall clock was rejected
  `time-too-far-ahead-of-mtp` (Y-F10, found mining testnet-yb-fast block 510,432 after a two-day
  pause). The node's `curtime` is also never below median-time-past + 1, which Y-F5 needed.
- `--share-ratio N` (default 1, unchanged behaviour): the miners' target is N times easier than the
  block's; a submit between the two is a share, answered `true`, counted in `/status` (`shares`,
  `shareRatio`), never forwarded, and the miner keeps its job. Without it a GPU that finds no
  block for three minutes is disconnected as silent (Y-F11, seen on testnet-yb-fast with a CPU
  miner at 20 s per nonce).
- A submit must carry the job's `ntime` (the pool offers no ntime rolling; ZIP 301 lets a server
  enforce it, and Equihash miners echo it): any other is refused `bad-ntime`, so a miner cannot move
  the header off the node's `curtime`.
- Duplicate submits are refused `duplicate`: each job remembers the SHA256d of up to 128 counted
  headers (at most 32 KiB per connection, 32 MiB at 1,024 connections, before the set's overhead), checked across the connection's last eight jobs; the share that fills a job moves the
  miner to a fresh one, and a later share on it is refused `job-full` (not counted against it).
- The Equihash solution of a submit that meets the block target is verified in the pool, off the
  reactor and under the submit-slot limit, before `submitblock`; an invalid one is refused
  `bad-solution` and never reaches the node (a peer could grind a garbage solution to the target
  with SHA256d alone and make the node verify it).
- The bad-submit count runs from the miner's last accepted block: duplicates, wrong `ntime`, bad
  solutions and blocks the node rejects count, except the race verdicts (`inconclusive`,
  `duplicate`, `duplicate-inconclusive`, `bad-prevblk`); every 32 accepted shares forgive one, so
  an honest card's occasional invalid solution never adds up to a disconnect, while a flood with
  fewer shares between refusals still does. A block is remembered as a duplicate only once the
  node has answered, so a retry after an RPC error goes through.

### CI

- The Semgrep scanner's OpenTelemetry pins move to 1.45.2 (GHSA-mf5v-w3w3-f6cg); CI tooling only.

## Unreleased — security CI

- Security CI (SECURITY-CI.md): Semgrep with yolo's own rules (no `yed_*` RPC, stock mining RPCs
  only, no unbounded socket reads, no panics on miner input), CodeQL (Rust, Actions), and
  `secops.yml` (zizmor, gitleaks, OSV-Scanner); Dependabot security updates only, grouped.
- The byte-array reads in `codec.rs`, `tx.rs` and `tag.rs` convert without `unwrap` (they were
  length-checked already; behaviour unchanged).
- rustls-webpki 0.103.16 (RUSTSEC-2026-0382), webpki-roots 1.0.10 (RUSTSEC-2026-0381).

## Unreleased — hardening (security audit 2026-10-01)

- Stratum reader bounded at 8 KiB per line; a longer line closes the socket (H-1).
- `--max-connections` (1024) and `--max-per-ip` (64); a socket that has not authorized
  within 30 s or a miner silent for three keepalive periods is closed; `/status` `miners`
  counts authorized miners only and a new `connections` gauge counts every socket (H-2).
- Every `mining.submit` is hash-checked against the target before `submitblock` (`high-hash`
  locally, nothing forwarded); at most eight `submitblock` calls in flight pool-wide; ten
  locally rejected submits in a row disconnect the miner; the template poll runs on a thread
  of its own, never behind queued submits (H-3). A submit for an unknown job id is rejected as
  `stale` instead of being matched to the latest job (H-17).
- One `mining.authorize` per connection, ten per minute per IP, `validateaddress` answers
  cached for ten minutes (H-4); the password compare is constant-time (H-5).
- `--no-flags` is refused unless the binary is built with `--features regtest`; `/status`
  reports `noFlags` (H-6). README and `/status` (`tagPayoutKey`) state that the tag's
  payoutKey is the node's whatever `--payout` says, with a startup warning without
  `--payout` (H-7).
- Miner-controlled strings are logged escaped and cut to 128 bytes (H-8).
- A startup warning when the RPC URL is plain http off loopback; a conf's `rpcbind=` is no
  longer used as the connect host (`rpcconnect=` is) (H-9). `--status-bind` defaults to
  `127.0.0.1` (H-10).
- Coinbase parser uses checked arithmetic on node-supplied lengths (H-17).
- Toolchain pinned to 1.91.0; GitHub CI (fmt, clippy, test, cargo audit, cargo deny) with
  actions pinned by commit; `rustfmt.toml` at the crate's 200-column style (H-16, I-7, I-8, I-13).

## Unreleased — ycashd 6.20.0

- The header root falls back to `getblocktemplate`'s `defaultroots` (`blockcommitmentshash`
  from NU5, `chainhistoryroot` from Heartwood) when ycashd 6.20.0 withholds the deprecated
  `lightclientroothash` / `finalsaplingroothash` (`-allowdeprecated=none`); before, the header
  was built with an empty root. A zero `chainhistoryroot` is never used (before Heartwood the
  field is the Sapling root, F-29): with no other key, no work is built and the error says why.
  v4.5.0 is unchanged.
- `tests/regtest.rs` generates the initial chain on a `-mocktime` start instead of
  `setmocktime` / `setmocktime 0` (refused, or the epoch, on 6.20.0), and runs on both
  ycashd v4.5.0 and 6.20.0; `YOLO_REGTEST_POOL_NODE_ARGS` passes extra arguments to the
  pool's node.

## v0.13.0 — 2026-09-28

One pool, no modes (owner decision P-6, plan Y7). `--mode solo|pool|cenote`, `--cenote N` and
`--scrooge` are gone; two flags decide the coinbase and the Yellowback tag is carried in every
combination:

- `--payout <s1…>` unset: each miner's stratum username is its payout address, validated with
  `validateaddress` on `mining.authorize` (`Invalid address` otherwise) — the `stratumpool`
  policy. Set: every block pays that address, validated once at startup, and the username is
  a worker name — `stratumsolo` / `cenote --scrooge`. Both cases rewrite `vout[0].scriptPubKey`
  structurally; there is no verbatim-`coinbasetxn` case any more (the old solo is
  `--payout <the node's mineraddress>` and produces the same bytes).
- `--text "…"` unset: the node's scriptSig untouched. Set: rebuilt as height push ‖
  `coinbaseaux.flags` ‖ push(text) within 100 bytes, with a truncation warning — the `cenote`
  policy. `--no-flags` stays hidden (test only, with `--text`). `--text` has no default any more.
- Job ids are the per-connection counter everywhere; `mining.extranonce.subscribe` is
  acknowledged only (the `stratumsolo` re-issue of the same job is dropped); `--port` defaults
  to 3333. Wire message shapes unchanged; the Perl fixtures replay (the solo fixture under
  `--payout`, the cenote ones under `--text`).
- `GET /status`: `mode` and `cenoteLeft` removed; `payout` (`"username"` or the fixed address)
  and `text` (bool) added.
- Library: `Config` carries `payout`, `text`, `no_flags` instead of `policy`/`cenote`;
  `bind()` resolves `--payout` against the node; `work::Policy { payout, text, no_flags }`,
  `work::Payout`; `work::Mode` removed.
- Tests: the unit tests are the payout × text grid; `tests/regtest.rs` mines one block per cell
  of the grid, plus `--no-flags` and the 100-byte boundary.
- After an accepted `submitblock` the poller is woken for a fresh template at once and no
  job is re-issued on the parent the node has just built past; previously the stale template
  was handed out under new job ids until the next 1 s poll and a fast solver's submits came
  back `inconclusive` (21 rejects over 3 blocks on the regtest devnet; the Perl had the same
  flaw). The 60 s keepalive re-notify skips a stale job too. Wire test
  `accepted_submit_refreshes_the_template_before_the_next_job`.

## v0.12.0 — 2026-09-28

The Rust rewrite. One binary, `yolo --mode solo|pool|cenote`, replaces the Perl
`stratumsolo`, `stratumpool` and `cenote` (moved to `legacy/perl/`, unchanged).

- Stratum wire format identical to the Perl's (`mining.subscribe/authorize/extranonce.subscribe/
  set_target/notify/submit`), replayed byte for byte against recorded exchanges in `tests/wire.rs`.
- JSON-RPC over HTTP to the node (`--rpc`, `--rpc-user/--rpc-password`, `--rpc-cookie`, `--conf`);
  no `ycash-cli` shell-outs. `--equihash auto` derives 48,5 (regtest) or 192,7 from the chain.
- The Ycash Yellowback (YED) coinbase tag survives every mode: `solo` and `pool` keep the node's
  scriptSig; `cenote` appends `coinbaseaux.flags` after the height push and truncates its text to
  the 100-byte scriptSig limit (Y-F1, Y-F8). Work is re-issued when the flags change (Y-F2).
- `mining.submit` is answered `true` only for a `null` `submitblock`; rejection strings are
  logged and counted (Y-F4). Job time is `max(template.curtime, now)` (Y-F5). `nonce1` is 14
  bytes as the Perl actually sent (Y-F6).
- `pool`/`cenote` rewrite the payout output through a v4 (Sapling) transaction parser, not a regex.
- `GET /status` (`--status-port`): mode, height, template age, miners, last tag kind, last verdict,
  accepted/rejected counters. The tag kind is logged on every work build.
- Tests: unit vectors from a real regtest block, the wire replay, and `cargo test --features
  regtest` (needs `YCASHD`): a two-node regtest driven by the Python `stratum-miner`, one block
  per case (`solo`, `pool`, `cenote`, `cenote --no-flags` → `found: false`, `cenote` at the
  100-byte boundary).
