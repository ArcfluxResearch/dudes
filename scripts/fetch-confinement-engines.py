#!/usr/bin/env python3
"""Download pinned native engines into a fresh proof directory."""

from pathlib import Path
import argparse
import json
import platform
import shutil
import tarfile
import urllib.request

from _engine_confinement import sha256


def download(url, destination, expected):
    with urllib.request.urlopen(url, timeout=30) as response, destination.open("xb") as target:
        shutil.copyfileobj(response, target)
    actual = sha256(destination)
    if actual != expected:
        raise ValueError("download checksum mismatch for " + destination.name)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    output = Path(args.output).resolve()
    output.mkdir(mode=0o700, parents=True, exist_ok=False)
    target = (platform.system(), platform.machine())
    pins = {
        ("Darwin", "arm64"): {
            "claude_platform": "darwin-arm64",
            "claude_sha256": "85d8c08707fcfd1e78ffa0204eefbc6ec9e4fe22f375fb2767ef85d957ed3161",
            "codex_platform": "aarch64-apple-darwin",
            "codex_archive_sha256": "a4587def33303e59992cb204ff442c0a2e7eef0d32c6ae5a6241a402db4950c2",
        },
        ("Linux", "x86_64"): {
            "claude_platform": "linux-x64",
            "claude_sha256": "86dc1fcc69498159eaf15f446613e847010f9036bfbbdffd4c49350be8489ada",
            "codex_platform": "x86_64-unknown-linux-musl",
            "codex_archive_sha256": "fba5a473f1fc044c620bca2d9bc26b1593284ffe358b68803ee09838633a1dc1",
        },
    }
    if target not in pins:
        raise ValueError("this platform has no pinned probe distribution")
    pin = pins[target]
    claude_url = "https://downloads.claude.ai/claude-code-releases/2.1.311/" + pin["claude_platform"] + "/claude"
    codex_url = "https://github.com/openai/codex/releases/download/rust-v0.164.0-alpha.11.1/codex-" + pin["codex_platform"] + ".tar.gz"
    claude = output / "claude"
    archive = output / "codex.tar.gz"
    codex = output / "codex"
    download(claude_url, claude, pin["claude_sha256"])
    download(codex_url, archive, pin["codex_archive_sha256"])
    member_name = "codex-" + pin["codex_platform"]
    with tarfile.open(archive) as source:
        members = [member for member in source.getmembers() if member.name == member_name and member.isfile()]
        if len(members) != 1:
            raise ValueError("the pinned archive does not contain its exact native binary")
        with source.extractfile(members[0]) as body, codex.open("xb") as destination:
            shutil.copyfileobj(body, destination)
    for binary in [claude, codex]:
        binary.chmod(0o700)
    receipt = {"platform": platform.platform(), "pins": pin, "sources": {"claude": claude_url, "codex": codex_url}}
    receipt["binary_sha256"] = {}
    for name, binary in [("claude", claude), ("codex", codex)]:
        receipt["binary_sha256"][name] = sha256(binary)
    (output / "downloads.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps(receipt, indent=2))


if __name__ == "__main__":
    main()
