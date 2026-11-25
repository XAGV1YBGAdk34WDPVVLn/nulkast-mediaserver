#!/usr/bin/env python3
"""Deterministic Test Harness for NulKast MediaServer.

Covers:
  1. Rust unit, API, RFC 9110 Range, and FFmpeg synthetic-video E2E tests (`cargo test`)
  2. Frontend JS syntax verification (`node --check web/app.js`)
  3. HTML <-> JS DOM ID contract verification (ensures every $('id') in app.js exists in index.html)
  4. Optional `--live <url>` smoke test against a running server
"""
import argparse
import json
import re
import shutil
import subprocess
import sys
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def check_rust_tests() -> bool:
    print("==> [1/3] Running Rust unit, API & FFmpeg E2E tests (cargo test)...")
    rc = subprocess.run(["cargo", "test"], cwd=ROOT).returncode
    return rc == 0


def check_js_syntax() -> bool:
    print("==> [2/3] Checking frontend JS syntax (web/app.js)...")
    if not shutil.which("node"):
        print("  [SKIP] node not found")
        return True
    rc = subprocess.run(["node", "--check", str(ROOT / "web" / "app.js")]).returncode
    if rc == 0:
        print("  [OK] web/app.js syntax valid")
    return rc == 0


def check_dom_contract() -> bool:
    print("==> [3/3] Verifying HTML <-> JS DOM ID contract...")
    html = (ROOT / "web" / "index.html").read_text()
    js = (ROOT / "web" / "app.js").read_text()
    html_ids = set(re.findall(r'\bid="([^"]+)"', html))
    js_ids = set(re.findall(r"\$\('([a-zA-Z0-9_-]+)'\)", js))
    missing = sorted(js_ids - html_ids)
    if missing:
        print(f"  [FAIL] Missing DOM IDs in web/index.html referenced by web/app.js: {missing}")
        return False
    print(f"  [OK] All {len(js_ids)} DOM IDs referenced in app.js exist in index.html")
    return True


def check_live(url: str) -> bool:
    print(f"==> [LIVE] Smoke-testing {url} ...")
    endpoints = [
        ("/api/health", "status"),
        ("/System/Info/Public", "StartupWizardCompleted"),
        ("/api/movies", "items"),
        ("/api/shows", "shows"),
        ("/api/diagnostics", "events"),
    ]
    for path, expected_key in endpoints:
        with urllib.request.urlopen(f"{url}{path}", timeout=10) as r:
            if r.status != 200:
                print(f"  [FAIL] {path} -> HTTP {r.status}")
                return False
            data = json.loads(r.read().decode())
            if expected_key not in data:
                print(f"  [FAIL] {path} missing key '{expected_key}'")
                return False
    # Verify static web bundle & local hls.min.js
    for asset in ("/web/index.html", "/web/app.js", "/web/style.css", "/web/hls.min.js"):
        with urllib.request.urlopen(f"{url}{asset}", timeout=10) as r:
            if r.status != 200:
                print(f"  [FAIL] {asset} -> HTTP {r.status}")
                return False
    print("  [OK] All live API, Jellyfin handshake & web bundle checks passed!")
    return True


def main() -> int:
    p = argparse.ArgumentParser(description="NulKast MediaServer Test Harness")
    p.add_argument("--live", default=None, help="Optional live server URL to smoke-test")
    args = p.parse_args()

    ok = check_rust_tests() and check_js_syntax() and check_dom_contract()
    if ok and args.live:
        ok = check_live(args.live.rstrip("/"))
    if ok:
        print("ALL CHECKS PASSED")
        return 0
    print("TEST HARNESS FAILED")
    return 1


if __name__ == "__main__":
    sys.exit(main())
