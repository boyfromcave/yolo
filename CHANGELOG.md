# Changelog

## Unreleased

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
