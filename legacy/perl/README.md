# The Perl originals

`stratumpool`, `stratumsolo` and `cenote` are ChileBob's StratumPool scripts as forked into
`yecdev/yolo`: single-process `IO::Select` stratum servers that shell out to `ycash-cli` for
`getblocktemplate` / `submitblock` and serve gminer, miniZ and lolMiner at Equihash 192/7. They
are kept here, unchanged, as the behavioural and wire-format reference the Rust `yolo` binary
was written against (`tests/fixtures/stratum-perl-*.jsonl` were recorded from them); their
history is in the workspace's read-only `ref/yolo` checkout (`main` @ `c9c155c6`). They are
not run, maintained or installed any more.

Why three scripts became one binary: they are one program copy-pasted three times. The git
history reads `stratumpool` (2020-10-17); `stratumsolo` two days later, which is `stratumpool`
minus the address check and the stats; `cenote` a month later, which is `stratumpool` plus
`--text`, a reward burn (`--cenote N`) and `--scrooge`, which is `stratumsolo` again. So
`cenote --scrooge` ≡ `stratumsolo` and `cenote` ≡ `stratumpool` + text, and the only real
choices are *who is paid* (each miner's username, or one fixed address) and *whether the
scriptSig is rebuilt with a text*. The Rust `yolo` is those two choices as two flags,
`--payout` and `--text` (owner decision P-6, plan Y7): `stratumpool` is `yolo`, `stratumsolo`
is `yolo --payout <the node's mineraddress>`, `cenote` is `yolo --text "…"` and
`cenote --scrooge` is `yolo --payout <mineraddress> --text "…"`. The burn was not carried over.

Why the rewrite (the Y-F rows of `docs/plans/role-pool-regtest-plan.md` §7):

- **Y-F1** — `cenote` rebuilds the coinbase scriptSig as height push + text (`cenote:522`), discarding `coinbaseaux.flags` and with it the Ycash Yellowback (YED) tag.
- **Y-F4** — every non-JSON `submitblock` outcome (`Block decode failed`, `time-too-old`, `high-hash`) is reported to the miner as accepted (`stratumsolo:117-126`).
- **Y-F5** — the header time is wall-clock `time()` (`stratumsolo:379`, `cenote:571`), not the template's `curtime`, so a chain whose median-time-past is ahead of the clock gets `time-too-old`.
- **Y-F6** — `nonce1` is 14 bytes (2-byte client index + 12 random), not the 16 the comment says (`stratumsolo:22,74`); miners size nonce2 from it.
- **Y-F8** — `stratumsolo` hardcodes port 3334; `cenote` assumes a 5-byte scriptSig (`cenote:522,527`), so on a tagging node its blocks do not even decode, and needs `mineraddress=` to be a wallet t-addr for its `ismine` scan.
