#!/usr/bin/env python3
"""Probe one real Claude binary with an isolated profile and a loopback model."""

from pathlib import Path
import argparse
import json
import platform
import subprocess
import time

from _engine_confinement import Action, Fixture, Model, command, network_denied, preflight, save, stop


def run(args):
    fixture = Fixture(Path(args.output).resolve(), "claude", args.inherited_hardlink)
    if not preflight(fixture, "claude"):
        return False
    paths = fixture.paths
    env = fixture.env
    env.update({
        "CLAUDE_CONFIG_DIR": str(paths["H"] / ".claude"),
        "XDG_CONFIG_HOME": str(paths["H"] / ".config"),
        "XDG_CACHE_HOME": str(paths["H"] / ".cache"),
        "XDG_STATE_HOME": str(paths["H"] / ".state"),
        "CLAUDE_CODE_TMPDIR": str(paths["T"]),
        "CLAUDE_CODE_SHELL": "/bin/bash",
        "ANTHROPIC_API_KEY": "local-only-dummy",
        "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST": "1",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL": "1",
        "CLAUDE_CODE_DISABLE_POLICY_SKILLS": "1",
        "CLAUDE_CODE_DISABLE_BACKGROUND_TASKS": "1",
        "CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK": "1",
        "CLAUDE_CODE_MAX_RETRIES": "0",
        "DISABLE_UPDATES": "1",
    })
    binary = Path(args.binary).resolve(strict=True)
    version = subprocess.run([str(binary), "--version"], env=env, cwd=paths["A"], capture_output=True, text=True, timeout=5).stdout.strip()
    model = Model(fixture, "claude", [])
    actions = []
    for name in ["A", "B", "C"]:
        target = paths[name] / "file-edit"
        (fixture.negative if name == "C" else fixture.positive).append(target)
        actions.append(Action("file-edit-" + name, "Edit", {"file_path": str(target), "old_string": "", "new_string": "local-proof"}))
    for index, base in enumerate(dict.fromkeys(p for p in fixture.g if p.is_dir())):
        target = base / "negative-edit"
        fixture.negative.append(target)
        actions.append(Action("metadata-edit-" + str(index), "Edit", {"file_path": str(target), "old_string": "", "new_string": "local-proof"}))
    for index, target in enumerate(p for p in fixture.g if p.is_file()):
        actions.append(Action("read-gitfile-" + str(index), "Read", {"file_path": str(target)}))
        actions.append(Action("edit-gitfile-" + str(index), "Edit", {"file_path": str(target), "old_string": target.read_text(), "new_string": "corrupt"}))
    actions.extend(fixture.shell_actions(model.endpoint))
    target = paths["C"] / "negative-unsandboxed"
    fixture.negative.append(target)
    actions.append(Action("unsandboxed-retry", "Bash", {"command": "/usr/bin/touch " + str(target), "description": "Unsandboxed retry", "dangerouslyDisableSandbox": True}))
    model.actions = actions
    env["ANTHROPIC_BASE_URL"] = model.endpoint
    if args.unavailable and platform.system() == "Linux":
        unavailable_path = fixture.root / "missing-sandbox-dependencies"
        unavailable_path.mkdir()
        env["PATH"] = str(unavailable_path)
    roots = [str(path) for path in fixture.roots]
    denied = [str(path) for path in fixture.protected]
    settings = {
        "disableAllHooks": True,
        "autoMemoryEnabled": False,
        "disableClaudeAiConnectors": True,
        "permissions": {
            "defaultMode": "dontAsk",
            "disableBypassPermissionsMode": "disable",
            "disableAutoMode": "disable",
            "allow": ["Edit(/" + path + "/**)" for path in roots],
            "deny": ["Edit(/" + path + suffix + ")" for path in denied for suffix in ["", "/**"]] + ["Bash(dangerouslyDisableSandbox:true)"],
        },
        "sandbox": {
            "enabled": True,
            "failIfUnavailable": True,
            "autoAllowBashIfSandboxed": True,
            "allowUnsandboxedCommands": False,
            "excludedCommands": [],
            "enableWeakerNestedSandbox": False,
            "enableWeakerNetworkIsolation": False,
            "allowAppleEvents": False,
            "allowNestedUserNamespaces": False,
            "filesystem": {"disabled": False, "allowWrite": roots, "denyWrite": denied},
            "network": {"allowedDomains": [], "strictAllowlist": True, "allowUnixSockets": [], "allowAllUnixSockets": False, "allowLocalBinding": False},
            "credentials": {"envVars": [{"name": "ANTHROPIC_API_KEY", "mode": "deny"}]},
        },
    }
    argv = command(binary, args.unavailable) + [
        "-p", "--input-format", "text", "--output-format", "stream-json", "--verbose",
        "--bare", "--restricted", "--permission-mode", "dontAsk", "--permission-prompts", "none",
        "--setting-sources", "", "--settings", json.dumps(settings),
        "--tools", "Bash,Read,Edit", "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}',
        "--disable-slash-commands", "--no-chrome", "--no-session-persistence", "--model", "claude-sonnet-4-6",
        "--max-turns", str(len(actions) + 2), "--add-dir", *roots[1:],
    ]
    if args.config_control:
        model.actions = []
        env.pop("ANTHROPIC_API_KEY")
        env.pop("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST")
        argv = command(binary, False) + [
            "-p", "--output-format", "stream-json", "--verbose", "--permission-mode", "dontAsk", "--permission-prompts", "none",
            "--tools", "", "--strict-mcp-config", "--mcp-config", str(paths["H"] / ".mcp.json"),
            "--disable-slash-commands", "--no-chrome", "--no-session-persistence", "--model", "claude-sonnet-4-6", "--max-turns", "2",
        ]
    started = time.monotonic()
    process = subprocess.Popen(argv, cwd=paths["A"], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, start_new_session=True)
    timed_out = False
    try:
        stdout, stderr = process.communicate("Run the local deterministic confinement probe.\n", timeout=40)
    except subprocess.TimeoutExpired:
        timed_out = True
        stop(process)
        stdout, stderr = process.communicate(timeout=3)
    finally:
        stop(process)
        model.close()
    (fixture.root / "stdout.jsonl").write_text(stdout)
    (fixture.root / "stderr.txt").write_text(stderr)
    if args.config_control:
        checks = {
            "expected_version": version == args.expect_version,
            "hook_positive_control": fixture.traps[0].exists(),
            "helper_positive_control": fixture.traps[1].exists(),
            "mcp_positive_control": fixture.traps[2].exists(),
            "local_model_completed": not timed_out and process.returncode == 0 and len(model.requests) == 1 and not model.errors,
        }
        return save(fixture, model, binary, argv, {"checks": checks, "version": version, "seconds": time.monotonic() - started, "config_control": True}, process)
    checks = fixture.checks(args.unavailable)
    offered = {tool.get("name") for row in model.requests for tool in row["body"].get("tools", [])}
    request_text = json.dumps(model.requests)
    unavailable_observed = "sandbox_apply: Operation not permitted" in request_text if platform.system() == "Darwin" else any(word in (stderr + stdout).lower() for word in ["bubblewrap", "bwrap", "socat"])
    exchange = not timed_out and process.returncode == 0 and len(model.requests) == len(actions) + 1 and not model.errors
    if args.unavailable and platform.system() == "Linux":
        exchange = not timed_out and not model.errors and process.returncode != 0 and not model.requests and unavailable_observed
    checks.update({
        "expected_version": version == args.expect_version,
        "complete_bounded_exchange": exchange,
        "closed_tools": (args.unavailable and not model.requests) or offered == {"Bash", "Read", "Edit"},
        "network_control_succeeds": fixture.network_control["exit"] == 0 and fixture.network_control["stdout"] == "local-network-control" and fixture.network_control["socket"]["exit"] == 0 and "NETWORK_CONNECTED" in fixture.network_control["socket"]["stdout"],
        "network_denied": network_denied(fixture, model, args.unavailable),
        "sandbox_unavailable_observed": not args.unavailable or unavailable_observed,
    })
    return save(fixture, model, binary, argv, {"checks": checks, "version": version, "seconds": time.monotonic() - started, "exit": process.returncode, "unavailable": args.unavailable, "inherited_hardlink": args.inherited_hardlink}, process)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--expect-version", default="2.1.311 (Claude Code)")
    parser.add_argument("--unavailable", action="store_true")
    parser.add_argument("--inherited-hardlink", action="store_true", help="arrange a diagnostic alias that preflight must refuse before launch")
    parser.add_argument("--config-control", action="store_true")
    raise SystemExit(0 if run(parser.parse_args()) else 1)
