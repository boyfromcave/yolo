# Changelog

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
