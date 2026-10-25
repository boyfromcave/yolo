# Security CI

yolo is network-facing (miners connect to its stratum port), so every push, PR and night runs the
security legs below. Only Semgrep and Dependabot ever open PRs, and few of them: at most one rolling PR
per Semgrep severity bucket and one grouped Dependabot security PR per ecosystem. Everything else
reports to *Security → Code scanning* (one category per tool) and the run's step summary.

| Leg | Workflow / job | Runs on | Gate | Results |
|---|---|---|---|---|
| Semgrep (semgrep-rules `rust`, `bash`, `dockerfile`, `generic/secrets`, `yaml/github-actions`; Trail of Bits `rs`, `generic`, `yaml/github-actions`; yolo's own `contrib/semgrep/rules/`) | `semgrep.yml` | PR (diff-aware), push, nightly 02:17 UTC | PR fails on a new **critical** finding | code scanning `semgrep`, rolling PRs `semgrep/<bucket>` |
| CodeQL (`rust`, `actions`; `security-extended`, build-mode none) | `codeql.yml` | PR, push to `upgrade/vault`, weekly | code scanning's own PR check (new alerts only) | code scanning `codeql/<language>` |
| zizmor (workflow security, default persona) | `secops.yml` `zizmor` | PR, push to `upgrade/vault`, nightly 03:43 UTC | fails on any finding | code scanning `zizmor` |
| gitleaks (secrets) | `secops.yml` `gitleaks` | PR (its commits), push/nightly/dispatch (full history) | fails on a leak not in `.gitleaksignore` | code scanning `gitleaks` (values redacted) |
| OSV-Scanner (`Cargo.lock`, hash-locked pip requirements) | `secops.yml` `osv-scanner` | PR, push, nightly | fails on a known vulnerability not in `osv-scanner.toml` | code scanning `osv-scanner` |
| cargo-deny (licences, sources, bans, advisories) + cargo-audit | `ci.yml` `audit` | every push and PR | fails on an advisory / policy breach | job log |
| Code-scanning triage (reviewed dismissals) | `code-scanning.yml` | push to `upgrade/vault` changing the dismissals file, nightly 03:53 UTC, dispatch (`dry_run`) | none; dismisses alerts listed in `contrib/secops/code-scanning-dismissals.json` | job log, artifact `code-scanning-alerts` |
| Dependabot | `.github/dependabot.yml` | GitHub | security updates only, grouped per ecosystem (cargo, github-actions, pip) | one PR per ecosystem |

## yolo's own Semgrep rules

`contrib/semgrep/rules/yolo.yaml`, tested by `contrib/semgrep/rules/yolo.rs`
(`semgrep --test contrib/semgrep/rules`):

- `yolo.rpc.yed-call` (critical): no `yed_*` RPC name in the pool's code; a pool uses only the stock
  mining RPCs and never needs a node change (`docs/plans/role-pool-regtest-plan.md` §6). The regtest
  harness under `tests/` drives its own nodes and is outside the rule.
- `yolo.rpc.non-stock-method` (high): `RpcClient::call` only with `getblocktemplate`, `submitblock`,
  `validateaddress`, `getblockchaininfo`.
- `yolo.net.unbounded-read` (high): no `read_line` / `read_until` / `read_to_end` / `read_to_string` on
  a stream in `src/`; socket input goes through `stratum::read_line_capped` (MAX_LINE) or a fixed buffer
  with a timeout.
- `yolo.stratum.panic-on-miner-input` (high): no `unwrap` / `expect` / `panic!`-family in the code that
  handles miner input (`stratum.rs`, `status.rs`, `equihash.rs`, `work.rs`, `tag.rs`), outside tests.

## Accepting a finding (always with a reason a reviewer can check)

- **Semgrep**: `contrib/semgrep/accepted.json` (by fingerprint or rule × paths), or `config.json`
  `exclude_rules` / `audit_rules` / `bucket_overrides` (contrib/semgrep/README.md).
- **OSV-Scanner**: an `[[IgnoredVulns]]` entry in `osv-scanner.toml` with `reason` and an `ignoreUntil`
  at most 90 days out.
- **gitleaks**: a fingerprint in `.gitleaksignore` with a comment, for test vectors and public
  constants only. A real secret is rotated, never ignored.
- **zizmor**: fix the workflow; an inline `# zizmor: ignore[<audit>]` only with the reason beside it.
- **Code-scanning alerts** (any tool): a false positive, test-only hit or accepted risk is an entry in
  `contrib/secops/code-scanning-dismissals.json` (tool, rule, path glob, reason, comment);
  `.github/workflows/code-scanning.yml` dismisses the matching alerts. A real finding is fixed, never listed.
- **cargo-deny / cargo-audit**: `deny.toml` (`[advisories] ignore`) with the reason.

## Repository settings (owner)

- *Settings → Code security*: code scanning is used by every SARIF upload (free for public repos);
  Dependabot security updates on.
- *Settings → Actions → General*: "Allow GitHub Actions to create and approve pull requests" for the
  Semgrep bucket PRs; optionally the secret `SEMGREP_PR_TOKEN` so CI runs on them
  (contrib/semgrep/README.md, "Repository settings").
