#!/usr/bin/env python3
"""Probe one real Codex app-server binary against a loopback Responses model."""

from pathlib import Path
import argparse
import json
import queue
import subprocess
import threading
import time

from _engine_confinement import Action, Fixture, Model, command, network_denied, preflight, save, stop, tool_result


def run(args):
    fixture = Fixture(Path(args.output).resolve(), "codex", args.inherited_hardlink)
    if not preflight(fixture, "codex"):
        return False
    paths = fixture.paths
    env = fixture.env
    env["CODEX_HOME"] = str(paths["profile"])
    binary = Path(args.binary).resolve(strict=True)
    version = subprocess.run([str(binary), "--version"], env=env, cwd=paths["A"], capture_output=True, text=True, timeout=5).stdout.strip()
    model = Model(fixture, "codex", [])
    model.actions = fixture.shell_actions(model.endpoint)
    for name, location, extra in [
        ("declared-workdir", "B", {"workdir": str(paths["B"])}),
        ("undeclared-workdir", "C", {"workdir": str(paths["C"])}),
        ("declared-pty", "A", {"tty": True}),
        ("undeclared-pty", "C", {"tty": True}),
        ("escalation-refused", "C", {"sandbox_permissions": "require_escalated", "justification": "Local refusal control"}),
    ]:
        target = paths[location] / name
        (fixture.negative if location == "C" else fixture.positive).append(target)
        model.actions.append(Action(name, "Bash", {"command": "/usr/bin/touch " + str(target), **extra}))
    roots = [str(path) for path in fixture.roots[1:]]
    options = {
        "features.hooks": "false", "features.plugins": "false", "features.apps": "false",
        "features.shell_snapshot": "false", "features.multi_agent": "false",
        "approval_policy": '"never"', "sandbox_mode": '"workspace-write"',
        "sandbox_workspace_write.exclude_tmpdir_env_var": "true", "sandbox_workspace_write.exclude_slash_tmp": "true",
        "sandbox_workspace_write.network_access": "false", "sandbox_workspace_write.writable_roots": json.dumps(roots),
        "model_provider": '"local"', "model": '"gpt-6.1"',
        "model_providers.local": '{name="local",base_url="' + model.endpoint + '/v1",wire_api="responses",requires_openai_auth=false,supports_websockets=false}',
        "web_search": '"disabled"', "mcp_servers": "{}", "sqlite_home": json.dumps(str(paths["profile"])),
        "log_dir": json.dumps(str(paths["profile"] / "log")),
    }
    ancestors = dict.fromkeys(str(path) for name in ["A", "B"] for path in [paths[name], *paths[name].parents])
    options["projects"] = "{" + ",".join(json.dumps(path) + '={trust_level="untrusted"}' for path in ancestors) + "}"
    if args.config_control:
        env["CODEX_HOME"] = str(paths["H"] / ".codex")
        model.actions = []
        options["features.hooks"] = "true"
        del options["mcp_servers"]
        options["projects"] = options["projects"].replace('"untrusted"', '"trusted"')
    argv = command(binary, args.unavailable) + ["app-server", "--listen", "stdio://", "--strict-config"]
    for name, value in options.items():
        argv.extend(["-c", name + "=" + value])
    stderr_file = (fixture.root / "stderr.txt").open("w")
    process = subprocess.Popen(argv, cwd=paths["A"], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr_file, text=True, start_new_session=True, bufsize=1)
    frames, received = [], queue.Queue()

    def read():
        for line in process.stdout:
            frame = json.loads(line)
            frames.append(frame)
            received.put(frame)
        received.put(None)

    reader = threading.Thread(target=read, daemon=True)
    reader.start()

    def send(frame):
        process.stdin.write(json.dumps(frame) + "\n")
        process.stdin.flush()

    def start_thread():
        send({"id": 2, "method": "thread/start", "params": {"cwd": str(paths["A"]), "approvalPolicy": "never", "sandbox": "workspace-write", "model": "gpt-6.1", "modelProvider": "local"}})

    policy = {"type": "workspaceWrite", "writableRoots": roots, "networkAccess": False, "excludeTmpdirEnvVar": True, "excludeSlashTmp": True}
    started = time.monotonic()
    completed, posture, managed_home, failed = False, False, False, None
    try:
        send({"id": 1, "method": "initialize", "params": {"clientInfo": {"name": "dudes", "title": "Local confinement probe", "version": "1"}, "capabilities": {"experimentalApi": True}}})
        while time.monotonic() - started < 40:
            try:
                frame = received.get(timeout=1)
            except queue.Empty:
                continue
            if frame is None:
                failed = "process exited before the completed turn"
                break
            if frame.get("error"):
                failed = frame["error"]
                break
            if frame.get("id") == 1:
                managed_home = frame.get("result", {}).get("codexHome") == env["CODEX_HOME"]
                if not managed_home:
                    break
                send({"method": "initialized"})
                if args.config_control:
                    send({"id": 4, "method": "hooks/list", "params": {"cwds": [str(paths["A"])]}})
                else:
                    start_thread()
            elif frame.get("id") == 4 and args.config_control:
                hooks = [hook for entry in frame["result"]["data"] for hook in entry.get("hooks", [])]
                trusted = {hook["key"]: {"trusted_hash": hook["currentHash"]} for hook in hooks}
                send({"id": 5, "method": "config/batchWrite", "params": {"edits": [{"keyPath": "hooks.state", "value": trusted, "mergeStrategy": "upsert"}], "filePath": None, "expectedVersion": None, "reloadUserConfig": True}})
            elif frame.get("id") == 5 and args.config_control:
                start_thread()
            elif frame.get("id") == 2:
                result = frame["result"]
                posture = result.get("cwd") == str(paths["A"]) and result.get("approvalPolicy") == "never" and result.get("sandbox") == policy and result.get("runtimeWorkspaceRoots") == [str(paths["A"]), *roots]
                if not posture:
                    failed = {"posture": result}
                    break
                send({"id": 3, "method": "turn/start", "params": {"threadId": result["thread"]["id"], "input": [{"type": "text", "text": "Run the local deterministic confinement probe.", "text_elements": []}], "cwd": str(paths["A"]), "approvalPolicy": "never", "sandboxPolicy": policy, "model": "gpt-6.1"}})
            elif frame.get("method") == "turn/completed":
                completed = frame.get("params", {}).get("turn", {}).get("status") == "completed"
                break
            elif frame.get("method") and frame.get("id") is not None:
                send({"id": frame["id"], "error": {"code": -32601, "message": "Interactive approval is refused"}})
    finally:
        stop(process)
        reader.join(timeout=2)
        stderr_file.close()
        model.close()
    (fixture.root / "stdout.jsonl").write_text("".join(json.dumps(frame) + "\n" for frame in frames))
    if args.config_control:
        checks = {
            "expected_version": version == args.expect_version,
            "hook_positive_control": fixture.traps[0].exists(),
            "mcp_positive_control": fixture.traps[2].exists(),
            "notify_positive_control": fixture.traps[3].exists(),
            "local_model_completed": completed and not failed and len(model.requests) == 1 and not model.errors,
        }
        return save(fixture, model, binary, argv, {"checks": checks, "version": version, "seconds": time.monotonic() - started, "config_control": True, "failure": failed}, process)
    checks = fixture.checks(args.unavailable)
    offered = {tool.get("name") for row in model.requests for tool in row["body"].get("tools", [])}
    allowed = {"exec_command", "write_stdin", "request_user_input", "view_image", "get_goal", "create_goal", "update_goal"}
    checks.update({
        "expected_version": version == args.expect_version,
        "managed_home_and_exact_posture": managed_home and posture,
        "complete_bounded_exchange": completed and not failed and len(model.requests) == len(model.actions) + 1 and not model.errors,
        "closed_tools": {"exec_command", "write_stdin"} <= offered <= allowed,
        "no_mcp_startup": not any("mcpserver" in str(frame.get("method", "")).lower() for frame in frames),
        "escalation_refused": "approval policy is Never" in tool_result(model, "escalation-refused"),
        "network_control_succeeds": fixture.network_control["exit"] == 0 and fixture.network_control["stdout"] == "local-network-control" and fixture.network_control["socket"]["exit"] == 0 and "NETWORK_CONNECTED" in fixture.network_control["socket"]["stdout"],
        "network_denied": network_denied(fixture, model, args.unavailable),
        "sandbox_unavailable_observed": not args.unavailable or "sandbox_apply: Operation not permitted" in json.dumps(model.requests),
    })
    return save(fixture, model, binary, argv, {"checks": checks, "version": version, "seconds": time.monotonic() - started, "failure": failed, "unavailable": args.unavailable, "inherited_hardlink": args.inherited_hardlink}, process)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--expect-version", default="codex-cli 0.164.0-alpha.11.1")
    parser.add_argument("--unavailable", action="store_true")
    parser.add_argument("--inherited-hardlink", action="store_true", help="arrange a diagnostic alias that preflight must refuse before launch")
    parser.add_argument("--config-control", action="store_true")
    raise SystemExit(0 if run(parser.parse_args()) else 1)
