#!/usr/bin/env python3
"""Deterministic control plane for NulKast MediaServer.

Reads optional overrides from local .env (gitignored) or environment variables:
  DEPLOY_HOST, REMOTE_DIR, SERVER_URL
"""
import argparse
import json
import os
import subprocess
import sys
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def load_dotenv(path: Path) -> dict[str, str]:
    vals: dict[str, str] = {}
    if not path.is_file():
        return vals
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, v = line.split("=", 1)
        vals[k.strip()] = v.strip().strip('"').strip("'")
    return vals


DOTENV = load_dotenv(ROOT / ".env")

DEPLOY_HOST = os.environ.get("DEPLOY_HOST") or DOTENV.get("DEPLOY_HOST", "")
REMOTE_DIR = os.environ.get("REMOTE_DIR") or DOTENV.get("REMOTE_DIR", "~/nulkast-mediaserver")
SERVER_URL = os.environ.get("SERVER_URL") or DOTENV.get("SERVER_URL") or DOTENV.get("PUBLIC_URL", "http://localhost:8275")


def run(cmd: list[str], cwd: Path | None = None) -> int:
    return subprocess.run(cmd, cwd=cwd).returncode


def http_json(url: str, method: str = "GET", payload: dict | None = None) -> dict:
    data = json.dumps(payload).encode() if payload is not None else None
    headers = {"Content-Type": "application/json"} if data else {}
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    with urllib.request.urlopen(req, timeout=15) as resp:
        return json.loads(resp.read().decode())


def cmd_test(_args) -> int:
    return run(["cargo", "test"], cwd=ROOT)


def cmd_deploy(_args) -> int:
    if not DEPLOY_HOST:
        return run(["docker", "compose", "up", "-d", "--build"], cwd=ROOT)
    rc = run(
        [
            "rsync", "-az", "--delete",
            "--exclude", "target",
            "--exclude", "data",
            "--exclude", ".git",
            "--exclude", ".env",
            "--exclude", ".agents",
            f"{ROOT}/", f"{DEPLOY_HOST}:{REMOTE_DIR}/",
        ]
    )
    if rc != 0:
        return rc
    return run(["ssh", DEPLOY_HOST, f"cd {REMOTE_DIR} && docker compose up -d --build"])


def cmd_health(args) -> int:
    url = args.url or SERVER_URL
    health = http_json(f"{url}/api/health")
    jf = http_json(f"{url}/System/Info/Public")
    print(json.dumps({"health": health, "jellyfin_handshake": jf}, indent=2))
    return 0


def cmd_scan(args) -> int:
    url = args.url or SERVER_URL
    res = http_json(f"{url}/api/scan", method="POST")
    print(json.dumps(res, indent=2))
    return 0


def cmd_logs(args) -> int:
    cmd = f"docker logs --tail {args.tail} nulkast-mediaserver"
    if DEPLOY_HOST:
        return run(["ssh", DEPLOY_HOST, cmd])
    return run(cmd.split())


def cmd_bench_vaapi(_args) -> int:
    cmd = "docker exec nulkast-mediaserver vainfo --display drm --device /dev/dri/renderD128 | head -n 30"
    if DEPLOY_HOST:
        return run(["ssh", DEPLOY_HOST, cmd])
    return run(["sh", "-c", cmd])


def main() -> int:
    p = argparse.ArgumentParser(description="NulKast MediaServer control CLI")
    sub = p.add_subparsers(dest="cmd", required=True)

    sub.add_parser("test").set_defaults(fn=cmd_test)
    sub.add_parser("deploy").set_defaults(fn=cmd_deploy)

    h = sub.add_parser("health")
    h.add_argument("--url", default=None)
    h.set_defaults(fn=cmd_health)

    s = sub.add_parser("scan")
    s.add_argument("--url", default=None)
    s.set_defaults(fn=cmd_scan)

    l = sub.add_parser("logs")
    l.add_argument("--tail", type=int, default=60)
    l.set_defaults(fn=cmd_logs)

    sub.add_parser("bench-vaapi").set_defaults(fn=cmd_bench_vaapi)

    args = p.parse_args()
    return args.fn(args)


if __name__ == "__main__":
    sys.exit(main())
