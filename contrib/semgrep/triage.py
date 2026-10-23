#!/usr/bin/env python3
# Copyright (c) 2026 The Ycash developers
# Distributed under the MIT software license, see the accompanying
# file COPYING or https://www.opensource.org/licenses/mit-license.php .
#
# Semgrep triage and severity-grouped PRs (contrib/semgrep/README.md).
#
#   triage   semgrep --json output -> findings.json + summary.md + results.sarif, one severity bucket
#            per finding (critical/high/medium/low), accepted findings dropped; --enforce exits 1 when a
#            bucket named in config.json "fail_on" is non-empty.
#   sync     findings.json -> at most one PR per bucket, on the bot-owned branch semgrep/<bucket>:
#            opted-in autofixes for that bucket's findings plus its ledger contrib/semgrep/findings/<bucket>.*
#            (first-seen date and SLA due date per finding). An empty bucket closes its PR.
#   preview  what sync would write (PR title, body, ledger) into files, without git or GitHub.
#
# Nothing this writes quotes a rule's own text (message, description, help): a finding names its rule
# and links to the rule's source at the pinned commit. The Semgrep Rules License allows use for one's
# own purposes but not redistribution, and these files end up public (PRs, artifacts, code scanning).
#
# Standard library only, so it runs on a runner and on a laptop alike.

import argparse
import datetime
import fnmatch
import hashlib
import json
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
BUCKETS = ["critical", "high", "medium", "low"]
LEDGER_DIR = "contrib/semgrep/findings"
BOT_NAME = "github-actions[bot]"
BOT_EMAIL = "41898282+github-actions[bot]@users.noreply.github.com"
PR_BODY_LIMIT = 60000
PR_TABLE_ROWS = 100

# Semgrep severities, old (ERROR/WARNING/INFO) and new (CRITICAL/HIGH/MEDIUM/LOW), to a bucket rank.
SEVERITY_RANK = {"CRITICAL": 3, "ERROR": 2, "HIGH": 2, "WARNING": 1, "MEDIUM": 1, "INFO": 0, "LOW": 0}
LEVEL = {"HIGH": 3, "MEDIUM": 2, "LOW": 1}


def load_json(path, default=None):
    if not os.path.exists(path):
        return default
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def load_config():
    return load_json(os.path.join(HERE, "config.json"))


def today():
    override = os.environ.get("SEMGREP_TRIAGE_TODAY")
    return datetime.date.fromisoformat(override) if override else datetime.date.today()


def matches(path, globs):
    return any(fnmatch.fnmatch(path, g) for g in globs)


class RuleSources:
    """Rule ids and source links. Semgrep names a rule <dotted directory of its file>.<id>, with the
    directory as given on the command line; that prefix is replaced by the source's id_prefix, and each
    rule links to its file and line at the source's pinned commit (or, for local rules, at --sha)."""

    def __init__(self, cfg, roots, repo, sha):
        # roots: source name -> checkout directory (from scan.py); the local rules live in this repo.
        self.sources = []  # (dotted prefixes, id_prefix, checkout dir, blob url base)
        for src in cfg["rule_sources"]:
            if src["name"] in roots:
                base = "%s/blob/%s/" % (src["repo"].rstrip("/").removesuffix(".git"), src["commit"])
                self.sources.append((roots[src["name"]], src["id_prefix"], base))
        local = "https://github.com/%s/blob/%s/" % (repo, sha) if repo else ""
        self.sources.append((cfg["local_rules"], "", local + cfg["local_rules"] + "/" if local else ""))
        self.links = {}
        for root, id_prefix, base in self.sources:
            for d, _, files in os.walk(root):
                for name in sorted(files):
                    if not name.endswith((".yaml", ".yml")):
                        continue
                    rel = os.path.relpath(os.path.join(d, name), root)
                    reldir = os.path.dirname(rel).replace(os.sep, ".")
                    with open(os.path.join(d, name), encoding="utf-8", errors="replace") as fh:
                        for n, text in enumerate(fh, 1):
                            stripped = text.strip()
                            if stripped.startswith("- id:"):
                                rid = stripped[5:].strip().strip("'\"")
                                full = id_prefix + (reldir + "." if reldir else "") + rid
                                if base:
                                    self.links.setdefault(full, "%s%s#L%d" % (base, rel.replace(os.sep, "/"), n))

    def rule_id(self, check_id):
        for root, id_prefix, _ in self.sources:
            for form in (root, os.path.abspath(root)):
                prefix = form.strip("/").replace("/", ".") + "."
                if check_id.startswith(prefix):
                    return id_prefix + check_id[len(prefix):]
        return check_id

    def link(self, rid):
        return self.links.get(rid, "")


def bucket_for(severity, meta, path, cfg):
    """critical/high/medium/low from rule severity, rule metadata and where the code lives.

    rank = severity (CRITICAL 3, ERROR/HIGH 2, WARNING/MEDIUM 1, INFO/LOW 0); an ERROR rule with HIGH
    impact and at least MEDIUM likelihood and confidence is critical; a LOW-confidence (audit-grade)
    rule drops one rank, and so does code that never ships in a release (non_production_paths). A local
    rule may pin its bucket with metadata.bucket; config.json bucket_overrides apply after this.
    """
    if meta.get("bucket") in BUCKETS:
        return meta["bucket"]
    rank = SEVERITY_RANK.get(str(severity).upper(), 0)
    impact = LEVEL.get(str(meta.get("impact", "")).upper(), 2)
    likelihood = LEVEL.get(str(meta.get("likelihood", "")).upper(), 2)
    confidence = LEVEL.get(str(meta.get("confidence", "")).upper(), 2)
    if rank == 2 and impact == 3 and likelihood >= 2 and confidence >= 2:
        rank = 3
    if confidence == 1:
        rank -= 1
    if matches(path, cfg["non_production_paths"]):
        rank -= 1
    return BUCKETS[3 - max(0, min(3, rank))]


def code_at(path, start, end):
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            lines = f.read().splitlines()
    except OSError:
        return ""
    return " ".join(" ".join(lines[start - 1:end]).split())


def triage(args):
    cfg = load_config()
    results = load_json(args.results)
    accepted = load_json(os.path.join(HERE, "accepted.json"), {"findings": {}, "rule_paths": []})
    sources = RuleSources(cfg, dict(r.split("=", 1) for r in args.rules_root or []), args.repo, args.sha)
    findings, audit, dropped, seen = [], [], {"category": 0, "excluded": 0, "accepted": 0}, {}
    for r in sorted(results["results"], key=lambda r: (r["path"], r["start"]["line"], r["check_id"])):
        rid = sources.rule_id(r["check_id"])
        meta = r["extra"].get("metadata", {})
        path = r["path"]
        if rid in cfg["exclude_rules"]:
            dropped["excluded"] += 1
            continue
        if meta.get("category") not in cfg["categories"]:
            dropped["category"] += 1
            continue
        # Line numbers move with unrelated edits; rule + path + the matched code does not.
        code = code_at(path, r["start"]["line"], r["end"]["line"])
        key = hashlib.sha256("\0".join((rid, path, code)).encode()).hexdigest()[:16]
        n = seen.get(key, 0)
        seen[key] = n + 1
        fingerprint = key if n == 0 else "%s-%d" % (key, n)
        if fingerprint in accepted["findings"] or any(
                fnmatch.fnmatch(rid, a["rule"]) and matches(path, a["paths"]) for a in accepted["rule_paths"]):
            dropped["accepted"] += 1
            continue
        bucket = bucket_for(r["extra"].get("severity", "INFO"), meta, path, cfg)
        for o in cfg.get("bucket_overrides", []):
            if fnmatch.fnmatch(rid, o["rule"]) and matches(path, o["paths"]):
                bucket = o["bucket"]
        f = {
            "fingerprint": fingerprint,
            "bucket": bucket,
            "rule": rid,
            "rule_url": sources.link(rid),
            "severity": r["extra"].get("severity"),
            "path": path,
            "line": r["start"]["line"],
            "end_line": r["end"]["line"],
            # Rules give cwe as a list or as one string; keep a list of unique entries.
            "cwe": list(dict.fromkeys([meta["cwe"]] if isinstance(meta.get("cwe"), str) else meta.get("cwe", []))),
        }
        # Autofix only rules a reviewer opted in (config.json autofix_rules), never in consensus or vendored
        # code: a pack's fix text is generic (it can add an import it does not write, or misorder arguments).
        if (r["extra"].get("fix") is not None and any(fnmatch.fnmatch(rid, g) for g in cfg["autofix_rules"])
                and not matches(path, cfg["autofix_never_paths"])):
            f["fix"] = {"start": r["start"]["offset"], "end": r["end"]["offset"], "text": r["extra"]["fix"]}
        # Audit-grade rules (config.json audit_rules) are leads for a reviewer, not defects: they go to
        # audit.md/audit.json for manual review and never to a bucket, a PR, code scanning or fail_on.
        if any(fnmatch.fnmatch(rid, g) for g in cfg.get("audit_rules", {})):
            f.pop("bucket")
            f.pop("fix", None)
            audit.append(f)
        else:
            findings.append(f)

    os.makedirs(args.out, exist_ok=True)
    with open(os.path.join(args.out, "findings.json"), "w", encoding="utf-8") as fh:
        json.dump(findings, fh, indent=1)
    counts = {b: sum(1 for f in findings if f["bucket"] == b) for b in BUCKETS}
    lines = ["## Semgrep findings%s" % (" (new versus %s)" % args.baseline if args.baseline else ""), "",
             "| Bucket | Findings | SLA |", "|---|---:|---|"]
    lines += ["| %s | %d | %d days |" % (b, counts[b], cfg["buckets"][b]["sla_days"]) for b in BUCKETS]
    lines += ["", "Dropped: %(category)d outside the scanned categories, %(excluded)d excluded rules, "
              "%(accepted)d accepted (contrib/semgrep/accepted.json)." % dropped,
              "Audit leads (config.json audit_rules; audit.md, no PR): %d." % len(audit),
              "Scan errors (files semgrep could not fully parse): %d." % len(results.get("errors", [])), ""]
    for b in BUCKETS:
        rows = [f for f in findings if f["bucket"] == b]
        if rows:
            lines += ["<details><summary>%s (%d)</summary>" % (b, len(rows)), "",
                      "| Rule | Location | Fingerprint |", "|---|---|---|"]
            lines += ["| %s | `%s:%d` | `%s` |" % (rule_cell(f), f["path"], f["line"], f["fingerprint"])
                      for f in rows[:PR_TABLE_ROWS]]
            if len(rows) > PR_TABLE_ROWS:
                lines.append("| ... %d more in findings.json | | |" % (len(rows) - PR_TABLE_ROWS))
            lines += ["", "</details>", ""]
    with open(os.path.join(args.out, "summary.md"), "w", encoding="utf-8") as fh:
        fh.write("\n".join(lines))
    with open(os.path.join(args.out, "results.sarif"), "w", encoding="utf-8") as fh:
        json.dump(sarif(findings, results), fh, indent=1)
    with open(os.path.join(args.out, "audit.json"), "w", encoding="utf-8") as fh:
        json.dump(audit, fh, indent=1)
    by_rule = {}
    for f in audit:
        by_rule.setdefault(f["rule"], []).append(f)
    alines = ["# Semgrep audit leads", "",
              "Rules in config.json `audit_rules` flag code worth a reviewer's look, not defects; they have no",
              "SLA and open no PR. %d leads from %d rules, most first." % (len(audit), len(by_rule)), "",
              "| Rule | Leads | Top paths |", "|---|---:|---|"]
    for rid, rows in sorted(by_rule.items(), key=lambda kv: -len(kv[1])):
        paths = {}
        for f in rows:
            paths[f["path"]] = paths.get(f["path"], 0) + 1
        top = ", ".join("`%s` (%d)" % kv for kv in sorted(paths.items(), key=lambda kv: -kv[1])[:3])
        alines.append("| %s | %d | %s |" % (rule_cell(rows[0]), len(rows), top))
    with open(os.path.join(args.out, "audit.md"), "w", encoding="utf-8") as fh:
        fh.write("\n".join(alines) + "\n")
    print(" ".join("%s=%d" % (b, counts[b]) for b in BUCKETS))
    failing = [b for b in cfg["fail_on"] if counts[b]]
    if args.enforce and failing:
        print("semgrep: %s findings (config.json fail_on)" % "/".join(failing), file=sys.stderr)
        return 1
    return 0


def rule_cell(f):
    return "[`%s`](%s)" % (f["rule"], f["rule_url"]) if f.get("rule_url") else "`%s`" % f["rule"]


# Code scanning's own severity scale (GitHub reads properties.security-severity), one value per bucket.
SARIF_SEVERITY = {"critical": ("error", "9.5"), "high": ("error", "8.0"), "medium": ("warning", "5.5"),
                  "low": ("note", "2.0")}


def sarif(findings, results):
    """SARIF 2.1.0 for code scanning, from the triaged findings: rule ids, links, CWE tags, buckets."""
    rules, order = {}, []
    for f in findings:
        if f["rule"] not in rules:
            level, sev = SARIF_SEVERITY[f["bucket"]]
            rule = {"id": f["rule"], "name": f["rule"], "shortDescription": {"text": f["rule"]},
                    "defaultConfiguration": {"level": level},
                    "properties": {"tags": list(dict.fromkeys(["security"] + f["cwe"])),
                                   "security-severity": sev}}
            if f.get("rule_url"):
                rule["helpUri"] = f["rule_url"]
                rule["fullDescription"] = {"text": "See the rule source: " + f["rule_url"]}
                rule["help"] = {"text": "Rule source: " + f["rule_url"],
                                "markdown": "Rule source: [%s](%s)" % (f["rule"], f["rule_url"])}
            rules[f["rule"]] = rule
            order.append(f["rule"])
    out = []
    for f in findings:
        level, _ = SARIF_SEVERITY[f["bucket"]]
        out.append({"ruleId": f["rule"], "ruleIndex": order.index(f["rule"]), "level": level,
                    "message": {"text": "%s (%s bucket). See the rule source for details." % (f["rule"], f["bucket"])},
                    "locations": [{"physicalLocation": {"artifactLocation": {"uri": f["path"], "uriBaseId": "%SRCROOT%"},
                                                        "region": {"startLine": f["line"], "endLine": f["end_line"]}}}],
                    "partialFingerprints": {"ycashSemgrep/v1": f["fingerprint"]}})
    version = results.get("version", "")
    return {"$schema": "https://json.schemastore.org/sarif-2.1.0.json", "version": "2.1.0",
            "runs": [{"tool": {"driver": {"name": "Semgrep Community Edition", "semanticVersion": version,
                                          "informationUri": "https://github.com/semgrep/semgrep",
                                          "rules": [rules[r] for r in order]}},
                      "results": out}]}


# ---------------------------------------------------------------------------------------------- sync

def run(cmd, cwd=None, check=True, input=None):
    p = subprocess.run(cmd, cwd=cwd, input=input, capture_output=True, text=True)
    if check and p.returncode:
        raise SystemExit("%s failed (%d):\n%s%s" % (" ".join(cmd), p.returncode, p.stdout, p.stderr))
    return p.stdout.strip()


class Gh:
    def __init__(self, repo, dry_run):
        self.repo, self.dry_run = repo, dry_run

    def read(self, *args):
        return run(["gh"] + list(args) + ["-R", self.repo])

    def write(self, *args):
        if self.dry_run:
            print("dry-run: gh %s" % " ".join(args))
            return ""
        return run(["gh"] + list(args) + ["-R", self.repo])


def ledger_state(ref, bucket):
    """fingerprint -> first_seen from a ledger at ref (the open PR's branch, or the scanned commit)."""
    out = subprocess.run(["git", "show", "%s:%s/%s.json" % (ref, LEDGER_DIR, bucket)],
                         capture_output=True, text=True)
    if out.returncode:
        return {}
    try:
        return {f["fingerprint"]: f["first_seen"] for f in json.loads(out.stdout)["findings"]}
    except (ValueError, KeyError, TypeError):
        return {}


def apply_fixes(worktree, findings):
    """Apply semgrep autofixes by byte offset, last first, skipping overlapping edits."""
    by_path = {}
    for f in findings:
        if "fix" in f:
            by_path.setdefault(f["path"], []).append(f["fix"])
    applied = 0
    for path, fixes in by_path.items():
        full = os.path.join(worktree, path)
        with open(full, "rb") as fh:
            data = fh.read()
        floor = len(data) + 1
        for fx in sorted(fixes, key=lambda x: x["start"], reverse=True):
            if fx["end"] > floor:
                continue
            data = data[:fx["start"]] + fx["text"].encode() + data[fx["end"]:]
            floor = fx["start"]
            applied += 1
        with open(full, "wb") as fh:
            fh.write(data)
    return applied


def blob_link(repo, sha, f):
    return "https://github.com/%s/blob/%s/%s#L%d" % (repo, sha, f["path"], f["line"])


def ledger_markdown(bucket, rows, sla, now):
    out = ["# Semgrep %s findings" % bucket, "",
           "Generated by contrib/semgrep/triage.py on the semgrep/%s branch; SLA %d days from first seen." % (bucket, sla),
           "Do not edit by hand: fix the code, or accept a finding in contrib/semgrep/accepted.json.", "",
           "| Status | Due | First seen | Rule | Location | Fingerprint |", "|---|---|---|---|---|---|"]
    for f in rows:
        status = "OVERDUE" if f["due"] < now.isoformat() else "open"
        out.append("| %s | %s | %s | %s | `%s:%d` | `%s` |" % (
            status, f["due"], f["first_seen"], rule_cell(f), f["path"], f["line"], f["fingerprint"]))
    return "\n".join(out) + "\n"


def pr_body(bucket, rows, sla, now, repo, sha, applied, run_url, human):
    overdue = [f for f in rows if f["due"] < now.isoformat()]
    oldest = min((f["due"] for f in rows), default="-")
    body = ["## Semgrep: %d %s finding%s" % (len(rows), bucket, "" if len(rows) == 1 else "s"), "",
            "| SLA | Earliest due | Overdue | Autofixes in this PR | Scanned commit |", "|---|---|---:|---:|---|",
            "| %d days | %s | %d | %d | %s |" % (sla, oldest, len(overdue), applied, sha[:12]), "",
            "This PR is maintained by the Semgrep workflow (contrib/semgrep/README.md): it is rebuilt from the "
            "latest scan, groups every `%s` finding into one place and closes itself when the bucket is "
            "empty. To clear a finding, fix it here or in any PR to the base branch, or accept it with a reason "
            "in `contrib/semgrep/accepted.json`. Merging lands the autofixes and the ledger "
            "`%s/%s.md` as the record of what is still open." % (bucket, LEDGER_DIR, bucket), ""]
    if human:
        body += ["> Commits by people are on this branch, so the workflow only refreshes the ledger here "
                 "and never force-pushes or closes it.", ""]
    body += ["Each rule links to its source, which explains the issue and the fix.", "",
             "| Status | Due | Rule | Location | CWE |", "|---|---|---|---|---|"]
    for f in rows[:PR_TABLE_ROWS]:
        cwe = ", ".join(c.split(":")[0] for c in f.get("cwe", []))
        body.append("| %s | %s | %s | [`%s:%d`](%s) | %s |" % (
            "**OVERDUE**" if f in overdue else "open", f["due"], rule_cell(f), f["path"], f["line"],
            blob_link(repo, sha, f), cwe))
    if len(rows) > PR_TABLE_ROWS:
        body.append("\n%d more in the ledger file." % (len(rows) - PR_TABLE_ROWS))
    if run_url:
        body += ["", "Scan run: %s" % run_url]
    text = "\n".join(body)
    return text[:PR_BODY_LIMIT]


def pr_title(bucket, rows, sla, overdue):
    if not rows:
        return "semgrep: no %s findings remain (ledger cleanup)" % bucket
    return "semgrep: %d %s finding%s, SLA %dd (due %s)%s" % (
        len(rows), bucket, "" if len(rows) == 1 else "s", sla, rows[0]["due"], " OVERDUE" if overdue else "")


def preview(args):
    """What sync would put in each bucket PR, written to files: no git, no GitHub."""
    cfg = load_config()
    findings = load_json(args.findings)
    now = today()
    os.makedirs(args.out, exist_ok=True)
    for bucket in BUCKETS:
        sla = cfg["buckets"][bucket]["sla_days"]
        rows = [f for f in findings if f["bucket"] == bucket]
        if not rows:
            print("%s: no findings, no PR" % bucket)
            continue
        ledger = load_json(os.path.join(LEDGER_DIR, bucket + ".json"), {"findings": []})
        first_seen = {f["fingerprint"]: f["first_seen"] for f in ledger["findings"]}
        for f in rows:
            f["first_seen"] = first_seen.get(f["fingerprint"], now.isoformat())
            f["due"] = (datetime.date.fromisoformat(f["first_seen"]) + datetime.timedelta(days=sla)).isoformat()
        rows.sort(key=lambda f: (f["due"], f["path"], f["line"]))
        overdue = any(f["due"] < now.isoformat() for f in rows)
        labels = cfg["pr_labels"] + [cfg["buckets"][bucket]["label"]] + ([cfg["overdue_label"]] if overdue else [])
        with open(os.path.join(args.out, bucket + "-pr.md"), "w", encoding="utf-8") as fh:
            fh.write("<!-- PR title: %s -->\n<!-- branch: %s%s -> %s; labels: %s -->\n\n" % (
                pr_title(bucket, rows, sla, overdue), cfg["branch_prefix"], bucket, cfg["base_branch"], ", ".join(labels)))
            fh.write(pr_body(bucket, rows, sla, now, args.repo, args.sha, 0, "", False) + "\n")
        with open(os.path.join(args.out, bucket + "-ledger.md"), "w", encoding="utf-8") as fh:
            fh.write(ledger_markdown(bucket, rows, sla, now))
        print("%s: %d findings -> %s-pr.md, %s-ledger.md" % (bucket, len(rows), bucket, bucket))
    return 0


def patch_id(a, b):
    diff = run(["git", "diff", a, b])
    return run(["git", "patch-id", "--stable"], input=diff + "\n").split(" ")[0] if diff else ""


def sync(args):
    cfg = load_config()
    findings = load_json(args.findings)
    now = today()
    sha = run(["git", "rev-parse", args.sha])
    gh = Gh(args.repo, args.dry_run)
    if not args.dry_run:
        for b in BUCKETS:
            gh.write("label", "create", cfg["buckets"][b]["label"], "--force", "--color", cfg["buckets"][b]["color"],
                     "--description", "Semgrep %s findings, SLA %d days" % (b, cfg["buckets"][b]["sla_days"]))
        gh.write("label", "create", cfg["overdue_label"], "--force", "--color", "000000",
                 "--description", "A Semgrep finding is past its SLA")
        for label in cfg["pr_labels"]:
            gh.write("label", "create", label, "--force", "--color", "5319e7")

    for bucket in BUCKETS:
        branch = cfg["branch_prefix"] + bucket
        sla = cfg["buckets"][bucket]["sla_days"]
        rows = [f for f in findings if f["bucket"] == bucket]
        remote = run(["git", "ls-remote", "origin", "refs/heads/" + branch]).split("\t")[0]
        human = False
        if remote:
            run(["git", "fetch", "--quiet", "origin", "+refs/heads/%s:refs/remotes/origin/%s" % (branch, branch)])
            authors = run(["git", "log", "--format=%ae", "%s..%s" % (sha, remote)]).split()
            human = any(a != BOT_EMAIL for a in authors)
        first_seen = ledger_state(sha, bucket)
        if remote:
            first_seen.update(ledger_state(remote, bucket))
        for f in rows:
            f["first_seen"] = first_seen.get(f["fingerprint"], now.isoformat())
            f["due"] = (datetime.date.fromisoformat(f["first_seen"]) + datetime.timedelta(days=sla)).isoformat()
        rows.sort(key=lambda f: (f["due"], f["path"], f["line"]))

        # Build the branch in a scratch worktree: the scanned commit (or, with people's commits on the
        # branch, that branch), this bucket's autofixes, and the ledger written or removed.
        tmp = tempfile.mkdtemp(prefix="semgrep-%s-" % bucket)
        start = remote if human else sha
        run(["git", "worktree", "add", "--quiet", "--detach", tmp, start])
        try:
            applied = 0 if human else apply_fixes(tmp, rows)
            ledger = os.path.join(tmp, LEDGER_DIR)
            os.makedirs(ledger, exist_ok=True)
            for ext in ("json", "md"):
                p = os.path.join(ledger, "%s.%s" % (bucket, ext))
                if os.path.exists(p):
                    os.remove(p)
            if rows:
                state = [{k: f[k] for k in ("fingerprint", "rule", "path", "line", "first_seen", "due")} for f in rows]
                with open(os.path.join(ledger, bucket + ".json"), "w", encoding="utf-8") as fh:
                    json.dump({"bucket": bucket, "sla_days": sla, "findings": state}, fh, indent=1)
                    fh.write("\n")
                with open(os.path.join(ledger, bucket + ".md"), "w", encoding="utf-8") as fh:
                    fh.write(ledger_markdown(bucket, rows, sla, now))
            run(["git", "add", "-A", "."], cwd=tmp)
            changed = run(["git", "status", "--porcelain"], cwd=tmp)
            if changed:
                run(["git", "-c", "user.name=" + BOT_NAME, "-c", "user.email=" + BOT_EMAIL, "commit", "--quiet",
                     "-m", "semgrep: %s findings (%d, SLA %d days)" % (bucket, len(rows), sla),
                     "-m", "Scanned %s. Autofixes applied: %d. Ledger: %s/%s.md." % (sha, applied, LEDGER_DIR, bucket)],
                    cwd=tmp)
            head = run(["git", "rev-parse", "HEAD"], cwd=tmp)
        finally:
            run(["git", "worktree", "remove", "--force", tmp], check=False)

        prs = json.loads(gh.read("pr", "list", "--head", branch, "--state", "open", "--json", "number,labels"))
        pr = prs[0]["number"] if prs else None
        empty = head == sha  # nothing differs from the scanned commit: no findings, no stale ledger

        if empty and not human:
            print("%s: clear" % bucket)
            if pr:
                gh.write("pr", "close", str(pr), "--delete-branch", "--comment",
                         "Semgrep scan of %s finds no open `%s` findings; closing." % (sha[:12], bucket))
            elif remote and not args.dry_run:
                run(["git", "push", "--quiet", "origin", "--delete", branch])
            continue
        if human and not rows and not pr:
            print("%s: clear (semgrep/%s has people's commits and no PR; left alone)" % (bucket, bucket))
            continue

        # Push only when the PR's change differs, so an unrelated base push does not rewrite the branch.
        if human:
            push = head != remote and ["git", "push", "--quiet", "origin", "%s:refs/heads/%s" % (head, branch)]
        else:
            same = remote and patch_id(sha, head) == patch_id(run(["git", "merge-base", sha, remote]), remote)
            push = not same and ["git", "push", "--quiet", "--force-with-lease=refs/heads/%s:%s" % (branch, remote),
                                 "origin", "%s:refs/heads/%s" % (head, branch)]
        if push:
            if args.dry_run:
                print("dry-run: %s" % " ".join(push))
            else:
                run(push)

        if not pr and not push and remote and json.loads(
                gh.read("pr", "list", "--head", branch, "--state", "closed", "--json", "number")):
            print("%s: PR closed by a person and nothing changed since; reopens when the findings change" % bucket)
            continue
        overdue = any(f["due"] < now.isoformat() for f in rows)
        title = pr_title(bucket, rows, sla, overdue)
        with tempfile.NamedTemporaryFile("w", suffix=".md", delete=False, encoding="utf-8") as fh:
            fh.write(pr_body(bucket, rows, sla, now, args.repo, sha, applied, args.run_url, human))
            body = fh.name
        labels = cfg["pr_labels"] + [cfg["buckets"][bucket]["label"]] + ([cfg["overdue_label"]] if overdue else [])
        if pr:
            gh.write("pr", "edit", str(pr), "--title", title, "--body-file", body, "--add-label", ",".join(labels))
            if not overdue and any(l["name"] == cfg["overdue_label"] for l in prs[0]["labels"]):
                gh.write("pr", "edit", str(pr), "--remove-label", cfg["overdue_label"])
            print("%s: %d findings, updated PR #%d%s" % (bucket, len(rows), pr, "" if push else " (branch unchanged)"))
        else:
            gh.write("pr", "create", "--base", cfg["base_branch"], "--head", branch, "--title", title,
                     "--body-file", body, "--label", ",".join(labels))
            print("%s: %d findings, opened a PR" % (bucket, len(rows)))
        os.remove(body)
    return 0


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    sub = ap.add_subparsers(dest="cmd", required=True)
    t = sub.add_parser("triage", help="bucket semgrep --json results")
    t.add_argument("--results", required=True)
    t.add_argument("--out", required=True)
    t.add_argument("--rules-root", action="append", metavar="NAME=DIR",
                   help="where config.json rule source NAME is checked out (repeatable; from scan.py)")
    t.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", ""), help="owner/name, for local rule links")
    t.add_argument("--sha", default=os.environ.get("GITHUB_SHA", "HEAD"), help="commit, for local rule links")
    t.add_argument("--baseline", help="the baseline ref of a diff-aware scan, for the summary heading")
    t.add_argument("--enforce", action="store_true", help="exit 1 when a fail_on bucket is non-empty")
    s = sub.add_parser("sync", help="open, update or close one PR per bucket")
    s.add_argument("--findings", required=True)
    s.add_argument("--sha", required=True, help="the commit that was scanned")
    s.add_argument("--repo", required=True, help="owner/name")
    s.add_argument("--run-url", default="")
    s.add_argument("--dry-run", action="store_true")
    v = sub.add_parser("preview", help="write what sync would put in each bucket PR, without git or GitHub")
    v.add_argument("--findings", required=True)
    v.add_argument("--out", required=True)
    v.add_argument("--repo", required=True, help="owner/name, for the links")
    v.add_argument("--sha", required=True, help="the commit that was scanned, for the links")
    args = ap.parse_args()
    return {"triage": triage, "sync": sync, "preview": preview}[args.cmd](args)


if __name__ == "__main__":
    sys.exit(main())
