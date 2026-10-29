#!/usr/bin/env python3
"""Run every local real-engine proof arm and preserve failures from each engine."""

from pathlib import Path
import argparse
import json
import platform
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binaries", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    scripts = Path(__file__).resolve().parent
    cases = [("claude", "--config-control"), ("claude", ""), ("claude", "--unavailable"), ("codex", "--config-control"), ("codex", "")]
    if platform.system() == "Darwin":
        cases.append(("codex", "--unavailable"))
    preflight = subprocess.run([sys.executable, str(scripts / "verify-engine-preflight.py"), "--output", args.output], timeout=10)
    results = [{"case": "preflight-only", "exit": preflight.returncode}]
    for engine, mode in cases:
        argv = [sys.executable, str(scripts / ("verify-" + engine + "-confinement.py")), "--binary", str(Path(args.binaries) / engine), "--output", args.output]
        if mode:
            argv.append(mode)
        done = subprocess.run(argv, timeout=90)
        results.append({"engine": engine, "mode": mode, "exit": done.returncode})
    output = Path(args.output)
    output.mkdir(parents=True, exist_ok=True)
    (output / "suite.json").write_text(json.dumps(results, indent=2) + "\n")
    print(json.dumps(results, indent=2))
    return all(result["exit"] == 0 for result in results)


if __name__ == "__main__":
    raise SystemExit(0 if main() else 1)
