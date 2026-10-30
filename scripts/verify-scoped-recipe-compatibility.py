#!/usr/bin/env python3
"""Check benign startup of Rust-generated unarmed recipes against loopback services.

Use only the pinned downloaded engines and a freshly built scoped_launch_fixture.
Receipts are compatibility observations, never production admission evidence.
Each arm starts a fresh process; none resumes a session or submits a second task.
"""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import argparse
import hashlib
import ipaddress
import json
import os
import platform
import queue
import socket
import subprocess
import tempfile
import threading
import time
import uuid
from urllib.parse import urlsplit

from _engine_confinement import inspect_writable_roots, sha256, stop

PINS = {
    ("Darwin", "arm64", "claude"): "85d8c08707fcfd1e78ffa0204eefbc6ec9e4fe22f375fb2767ef85d957ed3161",
    ("Darwin", "arm64", "codex"): "97026b7ea245a7a9f17d8522a2ba4a8cd2f1c01dac0f62483a4bfe427d76082e",
    ("Linux", "x86_64", "claude"): "86dc1fcc69498159eaf15f446613e847010f9036bfbbdffd4c49350be8489ada",
    ("Linux", "x86_64", "codex"): "e6d386e8822288f6d5f327e966a5a5e1de928de8795bb6149716005a410f38f2",
}
OUTER_NETWORK_GUARD = '(version 1)(allow default)(deny network-outbound)(allow network-outbound (remote ip "localhost:*"))(allow network-outbound (remote unix-socket))'
DUMMY_CREDENTIAL = "local-only-dummy"
CODEX_TOOL_TYPES = {"exec_command": "function", "write_stdin": "function", "request_user_input": "function", "apply_patch": "custom", "view_image": "function"}
NATIVE_GOAL_TOOLS = {"get_goal", "create_goal", "update_goal"}
MAX_HTTP_REQUESTS = 12
MAX_GET_REQUESTS = 10


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def closed_codex_tools(tools):
    return len(tools) == len(CODEX_TOOL_TYPES) and {tool.get("name"): tool.get("type") for tool in tools} == CODEX_TOOL_TYPES


def model_observation_checks(model, expected_posts):
    return {
        "model_server_error_free": not model.errors,
        "accepted_model_posts_exact": len(model.requests) == expected_posts,
        "attempted_model_posts_exact": model.post_attempts == expected_posts,
        "http_request_budget": model.http_total <= MAX_HTTP_REQUESTS and model.get_attempts <= MAX_GET_REQUESTS and model.http_overflow == 0,
    }


class LocalModel:
    def __init__(self, provider, target, action, unavailable):
        self.provider, self.target, self.action = provider, target, action
        self.requests, self.other, self.errors, self.credentials = [], [], [], []
        self.expected_posts = 2 if provider == "claude" or action == "declared-read" else 1
        self.http_attempts, self.http_total, self.get_attempts, self.post_attempts, self.http_overflow = [], 0, 0, 0, 0
        self.lock = threading.RLock()
        model = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def setup(self):
                super().setup()
                self.connection.settimeout(2)

            def handle_one_request(self):
                self.recorded = False
                super().handle_one_request()

            def observe(self):
                if getattr(self, "recorded", False):
                    return self.budget_allowed
                self.recorded = True
                self.budget_allowed = model.record_http(getattr(self, "command", None), getattr(self, "path", None), self.headers.get("Upgrade") if hasattr(self, "headers") else None)
                return self.budget_allowed

            def begin(self):
                if not self.observe():
                    self.send_error(429)
                    return False
                return True

            def send_error(self, code, message=None, explain=None):
                allowed = self.observe()
                model.fail("local HTTP error " + str(code))
                super().send_error(code, message, explain)
                if not allowed:
                    self.server.shutdown()

            def log_error(self, *_):
                self.observe()
                model.fail("local HTTP parser/transport error")

            def do_GET(self):
                if not self.begin():
                    return
                if self.path == "/v1/local-health":
                    self.send_response(200)
                    self.end_headers()
                    self.wfile.write(b"local-model-ready")
                    return
                if provider != "codex" or self.path != "/v1/responses" or self.headers.get("Upgrade", "").lower() != "websocket":
                    self.send_error(404)
                    return
                self.send_response(426)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(b'{"error":{"message":"loopback stand-in accepts SSE only"}}')

            def do_POST(self):
                if not self.begin():
                    return
                response_started = False
                try:
                    observed = [value for name, value in self.headers.items() if name.lower() in {"authorization", "x-api-key"}]
                    expected = {DUMMY_CREDENTIAL, "Bearer " + DUMMY_CREDENTIAL}
                    accepted = all(value in expected for value in observed)
                    model.credentials.append({"credential_headers_present": bool(observed), "all_present_credentials_are_explicit_dummy": accepted})
                    if not accepted:
                        model.fail("local endpoint refused unexpected credential selection")
                        self.send_error(401)
                        return
                    length = int(self.headers.get("Content-Length", "0"))
                    if not 0 < length <= 8 * 1024 * 1024:
                        raise ValueError("local model bound exceeded")
                    route = "/v1/messages" if provider == "claude" else "/v1/responses"
                    if self.path.split("?", 1)[0] != route:
                        self.send_error(404)
                        return
                    body = json.loads(self.rfile.read(length))
                    model.requests.append({"path": self.path, "body": body})
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.end_headers()
                    response_started = True
                    model.respond(self, body)
                except Exception as error:
                    model.fail(type(error).__name__ + " while serving local model response")
                    if not response_started:
                        self.send_error(400)

        class ModelServer(ThreadingHTTPServer):
            def handle_error(self, *_):
                model.fail("local HTTP handler exception")

        if platform.system() == "Darwin" and unavailable:
            class LoopbackServer(ModelServer):
                address_family = socket.AF_INET6
            self.server = LoopbackServer(("::1", 0), Handler)
            host = "localhost"
            if not all(ipaddress.ip_address(item[4][0]).is_loopback for item in socket.getaddrinfo(host, self.server.server_port)):
                raise ValueError("localhost resolution is not loopback-only")
        else:
            self.server = ModelServer(("127.0.0.1", 0), Handler)
            host = "127.0.0.1"
        self.server.daemon_threads = False
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.endpoint = "http://" + host + ":" + str(self.server.server_port) + "/v1"

    def fail(self, message):
        with self.lock:
            if message not in self.errors:
                self.errors.append(message)

    def record_http(self, method, path, upgrade):
        with self.lock:
            self.http_total += 1
            self.get_attempts += method == "GET"
            self.post_attempts += method == "POST"
            attempt = {"method": method, "path": path, "upgrade": upgrade}
            if len(self.http_attempts) <= MAX_HTTP_REQUESTS:
                self.http_attempts.append(attempt)
                if method == "GET":
                    self.other.append(attempt)
            else:
                self.http_overflow += 1
            allowed = self.http_total <= MAX_HTTP_REQUESTS and self.get_attempts <= MAX_GET_REQUESTS and self.post_attempts <= self.expected_posts
            if not allowed:
                self.fail("local HTTP/GET/POST request limit exceeded")
            return allowed

    @staticmethod
    def event(handler, kind, value):
        handler.wfile.write(("event: " + kind + "\ndata: " + json.dumps(value) + "\n\n").encode())
        handler.wfile.flush()

    def respond(self, handler, body):
        number = len(self.requests)
        emit = lambda kind, value: self.event(handler, kind, value)
        if self.provider == "claude":
            emit("message_start", {"type": "message_start", "message": {"id": "msg_local_" + str(number), "type": "message", "role": "assistant", "content": [], "model": body["model"], "stop_reason": None, "stop_sequence": None, "usage": {"input_tokens": 1, "output_tokens": 0}}})
            if number == 1:
                block = {"type": "tool_use", "id": "toolu_local_edit", "name": "Edit", "input": {}}
                delta = {"type": "input_json_delta", "partial_json": json.dumps({"file_path": str(self.target), "old_string": "", "new_string": "declared compatibility marker\n"})}
            else:
                block, delta = {"type": "text", "text": ""}, {"type": "text_delta", "text": "finished"}
            emit("content_block_start", {"type": "content_block_start", "index": 0, "content_block": block})
            emit("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": delta})
            emit("content_block_stop", {"type": "content_block_stop", "index": 0})
            emit("message_delta", {"type": "message_delta", "delta": {"stop_reason": "tool_use" if number == 1 else "end_turn", "stop_sequence": None}, "usage": {"output_tokens": 1}})
            emit("message_stop", {"type": "message_stop"})
        else:
            response = {"id": "resp_local_" + str(number), "object": "response", "created_at": 0, "status": "in_progress", "model": body.get("model"), "output": []}
            emit("response.created", {"type": "response.created", "response": response})
            if self.action == "declared-read" and number == 1:
                tools = {item.get("name"): item for item in body.get("tools", [])}
                schema = tools.get("exec_command", {}).get("parameters", {})
                if schema.get("required") != ["cmd"] or not {"cmd", "workdir", "login"} <= schema.get("properties", {}).keys():
                    raise ValueError("the advertised command schema does not match the bounded benign action")
                arguments = {"cmd": "/bin/cat ./sentinel", "workdir": str(self.target.parent), "login": False, "max_output_tokens": 128, "yield_time_ms": 1000}
                item = {"type": "function_call", "id": "fc_local_read", "call_id": "call_local_read", "name": "exec_command", "arguments": json.dumps(arguments), "status": "completed"}
            else:
                item = {"type": "message", "id": "msg_local_final", "status": "completed", "role": "assistant", "content": [{"type": "output_text", "text": "finished", "annotations": []}]}
            emit("response.output_item.added", {"type": "response.output_item.added", "output_index": 0, "item": item})
            emit("response.output_item.done", {"type": "response.output_item.done", "output_index": 0, "item": item})
            emit("response.completed", {"type": "response.completed", "response": {**response, "status": "completed", "output": [item], "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}})

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)


def fixture_input(root, provider, binary, endpoint):
    paths = {name: root / name for name in ["agent", "home", "profile", "copies", "origins"]}
    for path in paths.values():
        path.mkdir(mode=0o700)
    env = {"HOME": str(paths["home"]), "PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "GIT_CONFIG_NOSYSTEM": "1"}
    entries, git = [], []
    for index, name in enumerate(["a", "b", "c"]):
        origin, copy = paths["origins"] / name, paths["copies"] / name
        origin.mkdir(mode=0o700)
        def command(*argv):
            return subprocess.run(["/usr/bin/git", *argv], env=env, capture_output=True, text=True, check=True, timeout=5).stdout.strip()
        command("init", "-q", str(origin))
        (origin / "sentinel").write_text("declared fixture input\n")
        command("-C", str(origin), "add", "sentinel")
        command("-C", str(origin), "-c", "user.name=Local Probe", "-c", "user.email=probe@example.invalid", "commit", "-qm", "initial")
        command("-C", str(origin), "worktree", "add", "-q", str(copy), "--detach")
        gitdir = Path(command("-C", str(copy), "rev-parse", "--absolute-git-dir"))
        common = origin / ".git"
        identity = lambda path: {"device": path.stat().st_dev, "inode": path.stat().st_ino}
        entries.append({"repo_id": name, "value": {"origin": {"directory": str(origin), "directory_identity": identity(origin), "common_dir": str(common), "common_identity": identity(common)}, "execution": {"directory": str(copy), "directory_identity": identity(copy), "git_dir": str(gitdir), "git_identity": identity(gitdir)}, "worktree": f"{index:02x}" * 16, "base": command("-C", str(copy), "rev-parse", "HEAD"), "trust_lsn": 10}})
        git.append({"repo_id": name, "value": str(common)})
    private = {"home": str(paths["home"]), "profile": str(paths["profile"]), "work": str(paths["agent"] / "work"), "outbox": str(paths["agent"] / "outbox"), "temp": str(paths["agent"] / "tmp")}
    for key in ["work", "outbox", "temp"]:
        Path(private[key]).mkdir(mode=0o700)
    (paths["agent"] / "inbox").mkdir(mode=0o700)
    hired = {"project": {"id": "synthetic", "label": "Synthetic"}, "catalog_lsn": 20, "policy": "working_tree_v1", "repositories": {"primary": "a", "entries": entries}}
    grant = {"hired_lsn": 30, "project": "synthetic", "catalog_lsn": 20, "policy": "working_tree_v1", "repositories": {"primary": "b", "entries": [entries[1], entries[0]]}, "ticket": None}
    values = [("PATH", env["PATH"]), ("LANG", "en_US.UTF-8"), ("ANTHROPIC_API_KEY", DUMMY_CREDENTIAL), ("ANTHROPIC_BASE_URL", endpoint.removesuffix("/v1")), ("OPENAI_API_KEY", DUMMY_CREDENTIAL), ("CODEX_API_KEY", DUMMY_CREDENTIAL)]
    for index, value in enumerate([*private.values(), str(paths["home"] / ".config"), str(paths["home"] / ".cache"), str(paths["home"] / ".state"), "/bin/bash", "1", "0", "dumb", str(paths["agent"]), "file-mail"]):
        values.append(("SYNTHETIC_VALUE_" + str(index), value))
    return {"provider": provider, "config": {"command": str(binary), **({"permission_mode": "default"} if provider == "claude" else {"streaming": True})}, "private": private, "hired": hired, "grant": grant, "origin_git_dirs": git, "mode": "managed", "platform": "macos_arm64" if platform.system() == "Darwin" else "linux_x86_64", "legacy_cwd": str(paths["copies"] / "a"), "agent_dir": str(paths["agent"]), "session_kind": "first" if provider == "claude" else "assigned", "session_id": str(uuid.uuid4()), "model": "claude-sonnet-4-6" if provider == "claude" else "gpt-5.5", "task": "Complete the benign declared-root compatibility task.", "env": values}


def project(fixture, root, value):
    source, target = root / "fixture-input.json", root / "fixture-output.json"
    source.write_text(json.dumps(value, indent=2) + "\n")
    result = subprocess.run([str(fixture), str(source), str(target)], env={"HOME": value["private"]["home"], "PATH": "/usr/bin:/bin"}, capture_output=True, text=True, timeout=10)
    if result.returncode:
        raise RuntimeError("Rust fixture failed: " + result.stderr[-1000:])
    return json.loads(target.read_text())


def native_command(argv, unavailable):
    if platform.system() == "Darwin":
        return ["/usr/bin/sandbox-exec", "-p", OUTER_NETWORK_GUARD, *argv] if unavailable else argv
    routes = Path("/proc/net/route").read_text().splitlines()[1:]
    interfaces = {line.split(":", 1)[0].strip() for line in Path("/proc/net/dev").read_text().splitlines()[2:]}
    if interfaces != {"lo"} or any(line.split()[1] == "00000000" for line in routes):
        raise RuntimeError("Linux compatibility requires the isolated loopback-only network namespace")
    return argv


def inspect_selection(provider, binary, argv, env, policy, value, model):
    common = {"HOME", "PATH", "LANG", "SHELL", "TERM", "TMPDIR", "TMP", "TEMP", "DUDES_MAILBOX", "DUDES_COMMAND_TRANSPORT"}
    extra = {"CODEX_HOME", "CODEX_API_KEY", "OPENAI_API_KEY"} if provider == "codex" else {
        "ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL", "CLAUDE_CONFIG_DIR", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME",
        "CLAUDE_CODE_TMPDIR", "CLAUDE_CODE_SHELL", "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST", "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
        "CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL", "CLAUDE_CODE_DISABLE_POLICY_SKILLS", "CLAUDE_CODE_DISABLE_BACKGROUND_TASKS",
        "CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK", "CLAUDE_CODE_MAX_RETRIES", "DISABLE_UPDATES",
    }
    if set(env) != common | extra:
        raise ValueError("materialized environment is outside the closed synthetic allowlist")
    if env["DUDES_MAILBOX"] != value["agent_dir"] or env["DUDES_COMMAND_TRANSPORT"] != "file-mail":
        raise ValueError("the materialized mail transport differs from its owned fixture")
    names = {name for name in env if name.endswith("_API_KEY")}
    required = {"CODEX_API_KEY", "OPENAI_API_KEY"} if provider == "codex" else {"ANTHROPIC_API_KEY"}
    if names != required or any(env[name] != DUMMY_CREDENTIAL for name in names):
        raise ValueError("the materialized provider credentials are not exclusively explicit dummy values")
    private = value["private"]
    if env["HOME"] != private["home"] or env["TMPDIR"] != private["temp"] or env["TMP"] != private["temp"] or env["TEMP"] != private["temp"]:
        raise ValueError("the materialized home/temp selection is not the private fixture")
    profile = "CODEX_HOME" if provider == "codex" else "CLAUDE_CONFIG_DIR"
    if env[profile] != private["profile"]:
        raise ValueError("the materialized profile selection is not the private fixture")
    if any(any(Path(private[key]).iterdir()) for key in ["home", "profile"]):
        raise ValueError("provider state was not empty before launch")
    endpoint = urlsplit(model.endpoint)
    if endpoint.scheme != "http" or endpoint.hostname not in {"127.0.0.1", "localhost"} or endpoint.port != model.server.server_port or endpoint.path != "/v1" or endpoint.username or endpoint.password or endpoint.query or endpoint.fragment:
        raise ValueError("provider endpoint is not exactly the owned loopback listener")
    expected = list(policy["argv"])
    if provider == "claude":
        if env["ANTHROPIC_BASE_URL"] != model.endpoint.removesuffix("/v1"):
            raise ValueError("Claude endpoint selection differs from the owned listener")
    else:
        expected.extend(["-c", "openai_base_url=" + json.dumps(model.endpoint)])
    if argv != expected or argv[0] != str(binary) or sha256(binary) != PINS[(platform.system(), platform.machine(), provider)]:
        raise ValueError("effective program/argv differs from the pinned recipe and endpoint diagnostic")
    return {"program_pinned": True, "environment_closed": True, "credential_names": sorted(names), "only_explicit_dummy_credentials": True, "profile_initially_empty": True, "endpoint": model.endpoint, "endpoint_owned_loopback": True}


def run_case(args, case):
    provider = "codex" if case == "codex-declared-read" else case
    action = "declared-read" if case == "codex-declared-read" else "text"
    root = Path(tempfile.mkdtemp(prefix=case + "-", dir=args.output)).resolve()
    binary = Path(getattr(args, provider)).resolve(strict=True)
    if sha256(binary) != PINS.get((platform.system(), platform.machine(), provider)):
        raise RuntimeError("native binary does not match the exact pinned build")
    model = LocalModel(provider, root / "copies/b/compatibility-marker", action, args.macos_sandbox_unavailable)
    process = None
    started = time.monotonic()
    result = {"provider": provider, "case": case, "execution_mode": "sandbox-unavailable-control" if args.macos_sandbox_unavailable else "normal", "binary": str(binary), "binary_sha256": sha256(binary), "script_sha256": sha256(Path(__file__)), "platform": platform.platform(), "checks": {}, "error": None}
    try:
        value = fixture_input(root, provider, binary, model.endpoint)
        policy = project(args.fixture, root, value)
        (root / "generated-policy.json").write_text(json.dumps(policy, indent=2) + "\n")
        result["policy_sha256"] = digest(policy)
        result["candidate_version"] = policy["unarmed"]["candidate_version"]
        result["generated_argv"] = policy["argv"]
        result["fixture_sha256"] = sha256(args.fixture)
        result["model"] = value["model"]
        result["session_id"] = value["session_id"]
        result["preflight"] = inspect_writable_roots([Path(item["path"]) for item in policy["inventory"]["writable"]])
        if not result["preflight"]["accepted"]:
            raise RuntimeError("generated writable-root preflight refused")
        env = {item["name"]: value["env"][item["source"]][1] for item in policy["env"]}
        argv = list(policy["argv"])
        result["diagnostic_overrides"] = []
        if provider == "codex":
            override = "openai_base_url=" + json.dumps(model.endpoint)
            argv.extend(["-c", override])
            result["diagnostic_overrides"].append({"kind": "endpoint_only", "option": override})
        result["effective_argv_sha256"] = digest(argv)
        result["diagnostic_argv"] = argv
        result["environment_names"] = sorted(env)
        result["materialized_environment_sha256"] = digest(env)
        result["pre_launch_selection"] = inspect_selection(provider, binary, argv, env, policy, value, model)
        if platform.system() == "Darwin" and args.macos_sandbox_unavailable:
            result["outer_network_guard_sha256"] = digest(OUTER_NETWORK_GUARD)
            (root / "outer-network-guard.sb").write_text(OUTER_NETWORK_GUARD + "\n")
        health_args = ["/usr/bin/curl", "-q", "--max-time", "2", "--noproxy", "*", model.endpoint + "/local-health"]
        if platform.system() == "Darwin" and args.macos_sandbox_unavailable:
            health_args.insert(2, "-6")
        health = subprocess.run(native_command(health_args, args.macos_sandbox_unavailable), env={"HOME": value["private"]["home"], "PATH": "/usr/bin:/bin"}, capture_output=True, text=True, timeout=4)
        result["local_guard_positive_control"] = {"argv": health_args, "exit": health.returncode, "body": health.stdout, "stderr": health.stderr[-500:]}
        if health.returncode or health.stdout != "local-model-ready":
            raise RuntimeError("the local guard/endpoint positive control failed before engine launch")
        argv = native_command(argv, args.macos_sandbox_unavailable)
        result["spawn_argv_sha256"] = digest(argv)
        if provider == "claude":
            process = subprocess.Popen(argv, env=env, cwd=policy["cwd"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, start_new_session=True)
            timed_out = False
            try:
                stdout, stderr = process.communicate(value["task"], timeout=40)
            except subprocess.TimeoutExpired:
                stop(process)
                stdout, stderr = process.communicate(timeout=3)
                timed_out = True
            (root / "stdout.jsonl").write_text(stdout)
            (root / "stderr.txt").write_text(stderr)
            if timed_out:
                raise RuntimeError("bounded Claude compatibility timed out")
            rows = [json.loads(line) for line in stdout.splitlines() if line.startswith("{")]
            sessions = {row.get("session_id") for row in rows if row.get("session_id")}
            artifacts = []
            for base in [Path(value["private"]["home"]), Path(value["private"]["profile"])]:
                for path in base.rglob("*"):
                    if path.is_file() and path.stat().st_size <= 2 * 1024 * 1024 and value["session_id"].encode() in path.read_bytes():
                        session_user = False
                        if path.name == value["session_id"] + ".jsonl":
                            for line in path.read_text().splitlines():
                                row = json.loads(line)
                                session_user |= row.get("type") == "user" and row.get("sessionId") == value["session_id"] and row.get("message", {}).get("content") == value["task"]
                        artifacts.append({"path": str(path.relative_to(root)), "sha256": sha256(path), "bytes": path.stat().st_size, "contains_task": value["task"].encode() in path.read_bytes(), "recorded_user_task": session_user})
            result["persistence_artifacts"] = artifacts
            result["observed_sessions"] = sorted(sessions)
            result["native_initializations"] = [{key: row.get(key) for key in ["tools", "mcp_servers", "model", "claude_code_version", "permissionMode", "session_id"]} for row in rows if row.get("type") == "system" and row.get("subtype") == "init"]
            result["native_results"] = [{key: row.get(key) for key in ["subtype", "is_error", "result", "terminal_reason"]} for row in rows if row.get("type") == "result"]
            result["checks"] = {"exit_zero": process.returncode == 0, "recorded_session_exact": sessions == {value["session_id"]}, "closed_tool_inventory": any(set(item["tools"] or []) == {"Bash", "Read", "Edit"} for item in result["native_initializations"]), "declared_edit_observed": model.target.exists() and model.target.read_text() == "declared compatibility marker\n", "persistent_session_artifact": any(item["recorded_user_task"] for item in artifacts), "two_bounded_local_requests": len(model.requests) == 2 and not model.errors}
        else:
            result.update(run_codex(args, root, argv, env, value, policy, model))
    except Exception as error:
        result["error"] = str(error)
    finally:
        if process is not None:
            stop(process)
        model.close()
    observations = model_observation_checks(model, 2 if provider == "claude" or action == "declared-read" else 1)
    result["checks"].update(observations)
    if not all(observations.values()):
        failed = ", ".join(name for name, passed in observations.items() if not passed)
        result["error"] = "; ".join(filter(None, [result["error"], "local model observations failed: " + failed]))
    result.update({"scratch": str(root), "seconds": time.monotonic() - started, "local_model_requests": len(model.requests), "local_post_attempts": model.post_attempts, "local_http_attempts": model.http_total, "local_get_attempts": model.get_attempts, "http_attempt_log": model.http_attempts, "http_attempt_log_overflow": model.http_overflow, "http_limits": {"total": MAX_HTTP_REQUESTS, "get": MAX_GET_REQUESTS, "post": model.expected_posts}, "other_local_requests": model.other, "credential_observations": model.credentials, "model_errors": model.errors, "paid_model_calls": 0, "production_admission": "unarmed", "cleanup_observation": {"direct_child_exited": process.poll() is not None if process else result.get("direct_child_exited"), "model_accept_thread_stopped": not model.thread.is_alive(), "model_listener_closed": model.server.socket.fileno() == -1, "descendant_cleanup_verified": False}})
    result["egress_isolation"] = ("unavailable-control outer Seatbelt; prevents nested command sandboxing" if args.macos_sandbox_unavailable else "engine's own sandbox and explicit local dummy-provider controls; no outer loopback-egress isolation") if platform.system() == "Darwin" else "loopback-only ephemeral network namespace; no host sandbox settings altered"
    (root / "model-requests.json").write_text(json.dumps(model.requests, indent=2) + "\n")
    (root / "receipt.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({"receipt": str(root / "receipt.json"), "checks": result["checks"], "error": result["error"], "requests": len(model.requests)}, indent=2), flush=True)
    return result


def run_codex(args, root, argv, env, value, policy, model):
    stderr = (root / "stderr.txt").open("w")
    process = subprocess.Popen(argv, env=env, cwd=policy["cwd"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr, text=True, start_new_session=True, bufsize=1)
    frames, received, sent = [], queue.Queue(), []

    def reader():
        for line in process.stdout:
            try:
                frame = json.loads(line)
                frames.append(frame)
                received.put(frame)
            except json.JSONDecodeError:
                pass
        received.put(None)

    thread = threading.Thread(target=reader, daemon=True)
    thread.start()

    def send(message):
        process.stdin.write(json.dumps(message) + "\n")
        process.stdin.flush()
        sent.append(message)

    checks = {"catalog_profile_before_task": False, "thread_model_provider_exact": False, "rust_reader_accepted_native_posture": False, "task_completed": False, "closed_tool_inventory": False, "native_goal_tools_absent": False}
    result = {"checks": checks, "error": None}
    responses, emitted, catalog = [], 0, None
    expected_response, task_sent = 1, False
    deadline = time.monotonic() + 40
    try:
        initial = policy["protocol_actions"][0]
        if initial["kind"] != "write" or initial["task"] or initial["message"].get("method") != "initialize":
            raise RuntimeError("startup requires a non-task initialize action")
        send(initial["message"])
        emitted = 1
        while time.monotonic() < deadline:
            try:
                frame = received.get(timeout=1)
            except queue.Empty:
                continue
            if frame is None:
                raise RuntimeError("Codex exited before the compatibility turn finished")
            if frame.get("error"):
                raise RuntimeError("native protocol refusal: " + json.dumps(frame["error"]))
            if frame.get("id") is not None:
                if frame.get("method") is not None or frame["id"] != expected_response:
                    raise RuntimeError("unexpected native response phase; expected " + str(expected_response))
            if frame.get("id") == 1:
                responses.append(frame)
                expected_response = 90
                send({"method": "initialized"})
                send({"id": 90, "method": "model/list", "params": {}})
                continue
            if frame.get("id") == 90:
                models = frame.get("result", {}).get("data", [])
                catalog = next((entry for entry in models if entry.get("model", entry.get("id")) == "gpt-5.5"), None)
                result["catalog_ids"] = [entry.get("model", entry.get("id")) for entry in models]
                result["catalog_profile"] = catalog
                checks["catalog_profile_before_task"] = catalog is not None
                if catalog is None:
                    raise RuntimeError("endpoint override did not expose the recognized GPT-5.5 catalog profile")
                expected_response = 2
            elif frame.get("id") in [2, 3]:
                responses.append(frame)
                if frame.get("id") == 2:
                    body = frame.get("result", {})
                    result["native_thread_posture"] = body
                    checks["thread_model_provider_exact"] = body.get("model") == "gpt-5.5" and body.get("modelProvider") == "openai"
                    if not checks["thread_model_provider_exact"]:
                        raise RuntimeError("native thread changed the requested built-in provider/model")
                    expected_response = 3
                else:
                    expected_response = None
            elif frame.get("method") == "turn/completed":
                if not task_sent or expected_response is not None:
                    raise RuntimeError("native completion arrived before admitted task startup")
                checks["task_completed"] = frame.get("params", {}).get("turn", {}).get("status") == "completed"
                break
            else:
                continue
            replay = {**value, "vendor_responses": responses}
            generated = project(args.fixture, root, replay)
            if {key: item for key, item in generated.items() if key != "protocol_actions"} != {key: item for key, item in policy.items() if key != "protocol_actions"}:
                raise RuntimeError("replaying native responses changed the generated policy")
            actions = generated["protocol_actions"]
            for action in actions[emitted:]:
                if action["kind"] == "fail":
                    raise RuntimeError("unchanged Rust reader refused the native startup/posture")
                if action["kind"] == "write":
                    message = action["message"]
                    if bool(action["task"]) != (message.get("method") == "turn/start"):
                        raise RuntimeError("generated task marker disagrees with protocol method")
                    if message.get("method") != "initialized":
                        if action["task"]:
                            if catalog is None or not checks["catalog_profile_before_task"] or expected_response != 3 or task_sent:
                                raise RuntimeError("task dispatch requires admitted catalog and thread response")
                            task_sent = True
                            checks["rust_reader_accepted_native_posture"] = True
                        send(message)
            emitted = len(actions)
        else:
            raise RuntimeError("bounded Codex compatibility timed out")
        if model.requests:
            tools = model.requests[0]["body"].get("tools", [])
            result["offered_tools"] = tools
            names = [item.get("name", item.get("function", {}).get("name")) for item in tools]
            result["offered_tool_names"] = names
            inventories = [request["body"].get("tools", []) for request in model.requests]
            result["offered_tool_names_by_request"] = [[tool.get("name") for tool in inventory] for inventory in inventories]
            result["expected_tool_types"] = CODEX_TOOL_TYPES
            checks["closed_tool_inventory"] = all(closed_codex_tools(inventory) for inventory in inventories)
            checks["native_goal_tools_absent"] = all(not NATIVE_GOAL_TOOLS.intersection(inventory) for inventory in result["offered_tool_names_by_request"])
            result["effective_tool_profile"] = "Exact five-tool inventory with native goals disabled; custom apply_patch and recognized GPT-5.5 metadata retained. This is compatibility evidence, not complete confinement."
            checks["request_model_exact"] = all(request["body"].get("model") == "gpt-5.5" for request in model.requests)
    except Exception as error:
        result["error"] = str(error)
    finally:
        stop(process)
        thread.join(timeout=2)
        stderr.close()
    result["direct_child_exited"] = process.poll() is not None
    result["stdout_reader_thread_stopped"] = not thread.is_alive()
    result["task_frames_sent"] = sum(message.get("method") == "turn/start" for message in sent)
    checks["single_immutable_task"] = result["task_frames_sent"] == 1
    started_turns = [frame.get("params", {}).get("turn", {}).get("id") for frame in frames if frame.get("method") == "turn/started"]
    completed_turns = [frame.get("params", {}).get("turn", {}).get("id") for frame in frames if frame.get("method") == "turn/completed"]
    result["observed_turns"] = {"started": started_turns, "completed": completed_turns}
    checks["sole_task_turn_observed"] = len(started_turns) == 1 and started_turns == completed_turns
    checks["benign_completion_observed"] = any(frame.get("method") == "item/completed" and frame.get("params", {}).get("item", {}).get("type") == "agentMessage" and frame["params"]["item"].get("text") == "finished" for frame in frames)
    result["declared_command_action"] = model.action
    if model.action == "declared-read":
        outputs = [item for request in model.requests[1:] for item in request["body"].get("input", []) if item.get("type") == "function_call_output" and item.get("call_id") == "call_local_read"]
        result["declared_command_outputs"] = outputs
        checks["declared_read_observed"] = len(outputs) == 1 and "declared fixture input\n" in outputs[0].get("output", "")
        checks["declared_command_exit_zero"] = len(outputs) == 1 and "Process exited with code 0\n" in outputs[0].get("output", "")
    (root / "stdout.jsonl").write_text("".join(json.dumps(frame) + "\n" for frame in frames))
    (root / "sent-protocol.json").write_text(json.dumps(sent, indent=2) + "\n")
    return result


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--claude", required=True)
    parser.add_argument("--codex", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--case", choices=["claude", "codex", "codex-declared-read", "all"], default="all")
    parser.add_argument("--macos-sandbox-unavailable", action="store_true", help="separate negative control; never the normal macOS recipe path")
    args = parser.parse_args()
    if args.macos_sandbox_unavailable and platform.system() != "Darwin":
        parser.error("the explicit unavailable control is macOS-only")
    args.fixture = args.fixture.resolve(strict=True)
    args.output.mkdir(mode=0o700, parents=True, exist_ok=True)
    results = [run_case(args, case) for case in ([args.case] if args.case != "all" else ["claude", "codex", "codex-declared-read"])]
    (args.output / "summary.json").write_text(json.dumps(results, indent=2) + "\n")
    return all(result["checks"] and not result["error"] and all(result["checks"].values()) for result in results)


if __name__ == "__main__":
    raise SystemExit(0 if main() else 1)
