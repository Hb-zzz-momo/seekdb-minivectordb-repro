#!/usr/bin/env python3
"""Runs the organizers' public cases (mysqltest/) against the release binary
through their own checker, protocol_check/run.py: it waits for the ready
line, sends each statement as a JSON request, restarts on `--restart`
(shutdown request, then a new process on the same data directory), switches
`search_mode` on `--mode ann`, and compares every response with the case's
.result assertions the way the judge does.

  cargo build --release
  python3 tests/public_cases.py [--bin PATH] [--case ID ...] [--handshake-only]

The same checker against the Docker image, with the judge's container
options, is `python3 protocol_check/run.py --image IMAGE --output NEW_DIR`
(tests/docker_smoke.sh runs it).
"""
import argparse
import json
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=os.path.join(ROOT, "target/release/minivectordb"))
    ap.add_argument("--case", action="append", default=[])
    ap.add_argument("--handshake-only", action="store_true")
    a = ap.parse_args()
    out = os.path.join(ROOT, "target", "protocol-results", f"{time.strftime('%Y%m%d-%H%M%S')}-{os.getpid()}")
    cmd = [sys.executable, os.path.join(ROOT, "protocol_check", "run.py"),
           "--command", json.dumps([os.path.abspath(a.bin), "--data-dir", "{data_dir}"]), "--output", out]
    cmd += ["--handshake-only"] if a.handshake_only else [x for c in a.case for x in ("--case", c)]
    sys.exit(subprocess.run(cmd, cwd=ROOT).returncode)


if __name__ == "__main__":
    main()
