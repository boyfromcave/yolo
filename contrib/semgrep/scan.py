#!/usr/bin/env python3
# Copyright (c) 2026 The Ycash developers
# Distributed under the MIT software license, see the accompanying
# file COPYING or https://www.opensource.org/licenses/mit-license.php .
"""Run the Semgrep scan the CI workflow runs (.github/workflows/semgrep.yml), on a laptop or in CI.

  contrib/semgrep/scan.py [--baseline REF] [--enforce] [--out DIR] [--install]
  contrib/semgrep/scan.py --test [--install]

  --baseline REF  diff-aware: report only findings that are new versus REF (a PR's base)
  --enforce       exit 1 when a bucket named in config.json "fail_on" has findings
  --out DIR       where results.json, results.sarif, findings.json and summary.md go (default semgrep-out/)
  --install       when no semgrep of config.json "semgrep_version" is on PATH, install it into a venv
                  under the cache from contrib/semgrep/requirements.txt (hash-locked, CPython 3.12)
  --test          run the local rules' tests instead of a scan (`semgrep --test` over config.json
                  "local_rules": each rules file <name>.yaml beside its annotated test file <name>.<ext>)

Semgrep Community Edition only: the LGPL-2.1 `semgrep` package from PyPI, no login, no registry, no
Pro engine, --metrics=off. The rules are config.json "rule_sources" (each a git repository at a pinned
commit, never copied into this repository), fetched once into $SEMGREP_RULES_CACHE (default
~/.cache/ycash-semgrep), plus this repository's own config.json "local_rules".
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))


def run(cmd, **kw):
    return subprocess.run(cmd, check=True, **kw)


def semgrep_binary(cfg, cache, install):
    version = cfg["semgrep_version"]
    venv = os.path.join(cache, "venv-%s" % version)
    for candidate in (os.path.join(venv, "bin", "semgrep"), shutil.which("semgrep")):
        if candidate and os.path.exists(candidate):
            out = subprocess.run([candidate, "--version"], capture_output=True, text=True).stdout.strip()
            if out == version:
                return candidate
    if not install:
        sys.exit("scan.py: need semgrep %s: rerun with --install, or pip install semgrep==%s" % (version, version))
    print("scan.py: installing semgrep %s into %s" % (version, venv), flush=True)
    run([sys.executable, "-m", "venv", "--clear", venv])
    run([os.path.join(venv, "bin", "python"), "-m", "pip", "install", "--quiet", "--disable-pip-version-check",
         "--require-hashes", "--no-deps", "-r", os.path.join(HERE, "requirements.txt")])
    return os.path.join(venv, "bin", "semgrep")


def rules_checkout(src, cache):
    rules = os.path.join(cache, "%s-%s" % (src["name"], src["commit"]))
    if not os.path.isdir(rules):
        print("scan.py: fetching %s at %s" % (src["repo"], src["commit"]), flush=True)
        os.makedirs(cache, exist_ok=True)
        tmp = tempfile.mkdtemp(dir=cache)
        run(["git", "-C", tmp, "init", "-q"])
        run(["git", "-C", tmp, "fetch", "-q", "--depth", "1", src["repo"], src["commit"]])
        run(["git", "-C", tmp, "checkout", "-q", "FETCH_HEAD"])
        shutil.rmtree(os.path.join(tmp, ".git"))
        os.rename(tmp, rules)
    return rules


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--baseline")
    ap.add_argument("--enforce", action="store_true")
    ap.add_argument("--out", default="semgrep-out")
    ap.add_argument("--install", action="store_true")
    ap.add_argument("--test", action="store_true")
    args = ap.parse_args()

    os.chdir(subprocess.run(["git", "rev-parse", "--show-toplevel"], check=True, capture_output=True,
                            text=True, cwd=HERE).stdout.strip())
    with open(os.path.join(HERE, "config.json"), encoding="utf-8") as f:
        cfg = json.load(f)
    cache = os.environ.get("SEMGREP_RULES_CACHE") or os.path.expanduser("~/.cache/ycash-semgrep")
    semgrep = semgrep_binary(cfg, cache, args.install)
    if args.test:
        return subprocess.run([semgrep, "--test", "--metrics=off", "--disable-version-check",
                               cfg["local_rules"]]).returncode
    roots = {src["name"]: rules_checkout(src, cache) for src in cfg["rule_sources"]}

    os.makedirs(args.out, exist_ok=True)
    cmd = [semgrep, "scan", "--metrics=off", "--disable-version-check", "--oss-only",
           "--jobs", str(os.cpu_count() or 4), "--timeout", "30",
           "--json", "--output", os.path.join(args.out, "results.json")]
    for src in cfg["rule_sources"]:
        cmd += [a for p in src["paths"] for a in ("--config", os.path.join(roots[src["name"]], p))]
    cmd += ["--config", cfg["local_rules"]]
    if args.baseline:
        base = subprocess.run(["git", "rev-parse", args.baseline], check=True, capture_output=True, text=True)
        cmd += ["--baseline-commit", base.stdout.strip()]
    # semgrep exits 0 with or without findings here (no --error); the buckets decide (triage --enforce).
    rc = subprocess.run(cmd).returncode
    if rc:
        sys.exit("scan.py: semgrep failed (%d)" % rc)

    triage = [sys.executable, os.path.join(HERE, "triage.py"), "triage", "--results",
              os.path.join(args.out, "results.json"), "--out", args.out]
    triage += [a for name, root in roots.items() for a in ("--rules-root", "%s=%s" % (name, root))]
    if args.baseline:
        triage += ["--baseline", args.baseline]
    if args.enforce:
        triage.append("--enforce")
    return subprocess.run(triage).returncode


if __name__ == "__main__":
    sys.exit(main())
