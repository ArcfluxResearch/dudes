"""Shared scratch fixtures and bounded loopback services for real-engine probes."""

from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import hashlib
import json
import os
import platform
import re
import shlex
import signal
import stat
import subprocess
import sys
import tempfile
import threading
import time


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def inspect_writable_roots(roots, max_entries=100_000, timeout=2, max_depth=64):
    """Prototype metadata check; callers must exclude concurrent tree mutation."""
    started = time.monotonic()
    inspected = 0

    def bound(depth):
        if inspected > max_entries or depth > max_depth or time.monotonic() - started >= timeout:
            raise ValueError("writable-root inspection is incomplete because its bound was reached")

    def identity(metadata):
        return metadata.st_dev, metadata.st_ino, metadata.st_mtime_ns, metadata.st_ctime_ns

    def walk(descriptor, label, depth):
        nonlocal inspected
        bound(depth)
        before = os.fstat(descriptor)
        with os.scandir(descriptor) as entries:
            for entry in entries:
                inspected += 1
                bound(depth)
                metadata = os.stat(entry.name, dir_fd=descriptor, follow_symlinks=False)
                target = label / entry.name
                if stat.S_ISREG(metadata.st_mode) and metadata.st_nlink > 1:
                    raise ValueError("regular file has multiple hardlinks: " + str(target))
                if stat.S_ISDIR(metadata.st_mode):
                    child = os.open(entry.name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=descriptor)
                    try:
                        opened = os.fstat(child)
                        if (opened.st_dev, opened.st_ino) != (metadata.st_dev, metadata.st_ino):
                            raise ValueError("directory identity changed during inspection: " + str(target))
                        walk(child, target, depth + 1)
                    finally:
                        os.close(child)
        if identity(before) != identity(os.fstat(descriptor)):
            raise ValueError("directory changed during inspection: " + str(label))

    try:
        roots = [Path(root) for root in roots]
        if not roots:
            raise ValueError("no writable roots were supplied")
        for root in roots:
            if not root.is_absolute():
                raise ValueError("writable root must be absolute: " + str(root))
            if root.resolve(strict=True) != root:
                raise ValueError("writable root must be canonical: " + str(root))
            metadata = root.lstat()
            if not stat.S_ISDIR(metadata.st_mode):
                raise ValueError("writable root is not a directory: " + str(root))
            descriptor = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
            try:
                opened = os.fstat(descriptor)
                if (opened.st_dev, opened.st_ino) != (metadata.st_dev, metadata.st_ino):
                    raise ValueError("writable root identity changed: " + str(root))
                walk(descriptor, root, 0)
                if identity(opened) != identity(root.lstat()):
                    raise ValueError("writable root changed during inspection: " + str(root))
            finally:
                os.close(descriptor)
        bound(0)
        accepted, reason = True, None
    except (OSError, ValueError, RuntimeError) as error:
        accepted, reason = False, str(error)
    return {"accepted": accepted, "reason": reason, "inspected_entries": inspected, "roots": [str(root) for root in roots], "seconds": time.monotonic() - started, "prototype_only": True, "requires_quiescent_roots": True}


def preflight(fixture, engine):
    fixture.preflight = inspect_writable_roots(fixture.roots)
    (fixture.root / (engine + "-preflight.json")).write_text(json.dumps(fixture.preflight, indent=2) + "\n")
    if not fixture.preflight["accepted"]:
        print(json.dumps({"engine": engine, "preflight_refused": fixture.preflight, "engine_spawns": 0, "model_contacts": 0}, indent=2), flush=True)
    return fixture.preflight["accepted"]


@dataclass
class Action:
    name: str
    tool: str
    arguments: dict


class Fixture:
    def __init__(self, output, engine, inherited_hardlink=False):
        output.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.root = Path(tempfile.mkdtemp(prefix=engine + "-", dir=output)).resolve()
        self.paths = {name: self.root / name for name in ["A", "B", "C", "W", "O", "T", "H", "profile"]}
        for path in self.paths.values():
            path.mkdir(mode=0o700)
        self.env = {
            "HOME": str(self.paths["H"]),
            "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
            "TMPDIR": str(self.paths["T"]),
            "TMP": str(self.paths["T"]),
            "TEMP": str(self.paths["T"]),
            "SHELL": "/bin/bash",
            "TERM": "dumb",
        }
        self.gitdirs = []
        self.g = []
        self.git("init", "-q", str(self.paths["C"]))
        self.external = self.paths["C"] / "link-source"
        self.external.write_text("before\n")
        self.checkout_links = {}
        for name in ["A", "B"]:
            origin = self.root / ("origin" + name)
            origin.mkdir(mode=0o700)
            self.git("init", "-q", str(origin))
            os.link(self.external, origin / "tracked")
            self.git("-C", str(origin), "add", "tracked")
            self.git("-C", str(origin), "-c", "user.name=Local Probe", "-c", "user.email=probe@example.invalid", "commit", "-qm", "initial")
            self.git("-C", str(origin), "worktree", "add", "-q", str(self.paths[name]), "--detach")
            checkout = self.paths[name] / "tracked"
            self.checkout_links[name] = {
                "different_inode": checkout.stat().st_ino != self.external.stat().st_ino,
                "link_count": checkout.stat().st_nlink,
            }
            gitfile = self.paths[name] / ".git"
            gitdir = Path(gitfile.read_text().strip().split(": ", 1)[1]).resolve()
            self.gitdirs.append(gitdir)
            self.g.extend([gitfile, gitdir, origin / ".git"])
        self.roots = [self.paths[name] for name in ["A", "B", "W", "O", "T"]]
        self.protected = self.g + [self.paths["H"], self.paths["profile"]]
        self.before = {str(path): path.read_bytes() for path in self.g if path.is_file()}
        self.network_control = None
        self.network_namespace = os.readlink("/proc/self/ns/net") if platform.system() == "Linux" else None
        self.negative = []
        self.positive = []
        if inherited_hardlink:
            os.link(self.external, self.paths["A"] / "inherited-link")
        self.traps = [self.root / name for name in ["hook-ran", "helper-ran", "mcp-ran", "notify-ran"]]
        self.seed_configuration()
        self.network_script = self.paths["W"] / "network-probe.py"
        self.network_script.write_text(
            "import socket, sys\n"
            + "connection = socket.create_connection(('127.0.0.1', int(sys.argv[1])), timeout=2)\n"
            + "print('NETWORK_CONNECTED')\n"
            + "connection.sendall(('GET ' + sys.argv[2] + ' HTTP/1.0\\r\\nHost: localhost\\r\\n\\r\\n').encode())\n"
            + "print(connection.recv(1024).decode())\n"
        )

    def git(self, *args):
        subprocess.run(["/usr/bin/git", *args], env=self.env, stdin=subprocess.DEVNULL, capture_output=True, check=True, timeout=5)

    def seed_configuration(self):
        helper = self.root / "api-key-helper.sh"
        helper.write_text("#!/bin/sh\n/usr/bin/touch " + shlex.quote(str(self.traps[1])) + "\nprintf local-only-dummy\n")
        helper.chmod(0o700)
        mcp = self.root / "mcp-canary.py"
        mcp.write_text(
            "#!" + sys.executable + "\nimport json, pathlib, sys\n"
            + "pathlib.Path(" + repr(str(self.traps[2])) + ").touch()\n"
            + "for line in sys.stdin:\n"
            + "    value = json.loads(line)\n"
            + "    if 'id' not in value: continue\n"
            + "    result = {'protocolVersion': value.get('params', {}).get('protocolVersion', '2025-06-18'), 'capabilities': {'tools': {}}, 'serverInfo': {'name': 'local-canary', 'version': '1'}} if value.get('method') == 'initialize' else {'tools': []}\n"
            + "    print(json.dumps({'jsonrpc':'2.0', 'id':value['id'], 'result':result}), flush=True)\n"
        )
        mcp.chmod(0o700)
        for base in [self.paths["A"], self.paths["B"], self.paths["H"]]:
            claude = base / ".claude"
            codex = base / ".codex"
            claude.mkdir(exist_ok=True)
            codex.mkdir(exist_ok=True)
            hook = "/usr/bin/touch " + shlex.quote(str(self.traps[0]))
            (claude / "settings.json").write_text(json.dumps({
                "hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": hook}]}]},
                "apiKeyHelper": str(helper),
            }))
            (base / ".mcp.json").write_text(json.dumps({"mcpServers": {"probe": {"command": str(mcp), "args": []}}}))
            (codex / "config.toml").write_text(
                "notify = " + json.dumps(["/usr/bin/touch", str(self.traps[3])]) + "\n"
                + "[mcp_servers.probe]\ncommand = " + json.dumps(str(mcp)) + "\nargs = []\n"
            )
            (codex / "hooks.json").write_text(json.dumps({"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": hook}]}]}}))

    def shell_actions(self, endpoint):
        actions = []

        def shell(name, command):
            actions.append(Action(name, "Bash", {"command": command, "description": name}))

        for name in ["A", "B", "W", "O", "T"]:
            target = self.paths[name] / "positive-shell"
            self.positive.append(target)
            shell("declared-" + name, "/usr/bin/touch " + shlex.quote(str(target)))
        for index, base in enumerate(dict.fromkeys([self.paths["C"], *(p for p in self.g if p.is_dir()), self.paths["H"], self.paths["profile"]])):
            target = base / "negative-shell"
            self.negative.append(target)
            shell("protected-" + str(index), "/usr/bin/touch " + shlex.quote(str(target)))
        for index, gitfile in enumerate(p for p in self.g if p.is_file()):
            shell("gitfile-" + str(index), "printf corrupt > " + shlex.quote(str(gitfile)))
            shell("gitfile-rename-" + str(index), "mv " + shlex.quote(str(gitfile)) + " " + shlex.quote(str(gitfile.with_name("gitfile-moved"))))
        alias = self.paths["A"] / "alias"
        alias.symlink_to(self.paths["C"], target_is_directory=True)
        self.negative.append(self.paths["C"] / "negative-alias")
        shell("symlink", "/usr/bin/touch " + shlex.quote(str(alias / "negative-alias")))
        shell("create-hardlink", "ln " + shlex.quote(str(self.external)) + " " + shlex.quote(str(self.paths["A"] / "created-link")) + " && printf after > " + shlex.quote(str(self.paths["A"] / "created-link")))
        shell("rename-outside", "mv " + shlex.quote(str(self.external)) + " " + shlex.quote(str(self.paths["A"] / "moved-source")))
        shell("checkout-independent", "printf checkout > " + shlex.quote(str(self.paths["A"] / "tracked")))
        for name in ["A", "C"]:
            target = self.paths[name] / "interpreter-write"
            (self.positive if name == "A" else self.negative).append(target)
            code = "from pathlib import Path; Path(" + repr(str(target)) + ").write_text('probe')"
            shell("interpreter-" + name, shlex.quote(sys.executable) + " -c " + shlex.quote(code))
        shell("network-denied", "/usr/bin/curl --max-time 2 --noproxy '*' " + shlex.quote(endpoint + "/network-probe"))
        shell("socket-denied", shlex.quote(sys.executable) + " " + shlex.quote(str(self.network_script)) + " " + endpoint.rsplit(":", 1)[1] + " /network-probe")
        if platform.system() == "Linux":
            shell("network-namespace", "/usr/bin/readlink /proc/self/ns/net")
        return actions

    def checks(self, unavailable):
        return {
            "fresh_git_copies_break_origin_hardlinks": all(v["different_inode"] and v["link_count"] == 1 for v in self.checkout_links.values()),
            "declared_writes": all(p.exists() for p in self.positive) if not unavailable else not any(p.exists() for p in self.positive if p.name != "file-edit"),
            "undeclared_writes_refused": not any(p.exists() for p in self.negative),
            "gitfiles_unchanged": all(Path(p).is_file() and Path(p).read_bytes() == value for p, value in self.before.items()),
            "outside_inode_unchanged": self.external.exists() and self.external.read_text() == "before\n",
            "hooks_helpers_mcp_excluded": not any(p.exists() for p in self.traps),
        }


class Model:
    def __init__(self, fixture, protocol, actions):
        self.fixture, self.protocol, self.actions = fixture, protocol, actions
        self.requests, self.other_requests, self.errors = [], [], []
        model = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_GET(self):
                model.other_requests.append({"method": "GET", "path": self.path})
                self.send_response(200 if self.path == "/network-control" else 403)
                self.end_headers()
                if self.path == "/network-control":
                    self.wfile.write(b"local-network-control")

            def do_POST(self):
                try:
                    length = int(self.headers.get("Content-Length", "0"))
                    if not 0 < length <= 8 * 1024 * 1024 or len(model.requests) >= 128:
                        raise ValueError("bounded local model input exceeded")
                    body = json.loads(self.rfile.read(length))
                    model.requests.append({"path": self.path, "body": body})
                    allowed = "/v1/messages" if model.protocol == "claude" else "/v1/responses"
                    if self.path.split("?", 1)[0] != allowed:
                        raise ValueError("unexpected local model route")
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.end_headers()
                    model.respond(self, body)
                except Exception as error:
                    model.errors.append(repr(error))

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.endpoint = "http://127.0.0.1:" + str(self.server.server_port)
        control = subprocess.run(["/usr/bin/curl", "--max-time", "2", "--noproxy", "*", self.endpoint + "/network-control"], env=fixture.env, cwd=fixture.paths["A"], capture_output=True, text=True, timeout=4)
        fixture.network_control = {"exit": control.returncode, "stdout": control.stdout, "stderr": control.stderr}
        socket_control = subprocess.run([sys.executable, str(fixture.network_script), str(self.server.server_port), "/network-control"], env=fixture.env, cwd=fixture.paths["A"], capture_output=True, text=True, timeout=4)
        fixture.network_control["socket"] = {"exit": socket_control.returncode, "stdout": socket_control.stdout, "stderr": socket_control.stderr}

    @staticmethod
    def event(handler, kind, data):
        handler.wfile.write(("event: " + kind + "\ndata: " + json.dumps(data) + "\n\n").encode())
        handler.wfile.flush()

    def respond(self, handler, body):
        number = len(self.requests)
        action = self.actions[number - 1] if number <= len(self.actions) else None
        event = lambda kind, data: self.event(handler, kind, data)
        if self.protocol == "claude":
            event("message_start", {"type": "message_start", "message": {"id": "msg_local_" + str(number), "type": "message", "role": "assistant", "content": [], "model": body["model"], "stop_reason": None, "stop_sequence": None, "usage": {"input_tokens": 1, "output_tokens": 0}}})
            block = {"type": "tool_use", "id": "toolu_local_" + str(number), "name": action.tool, "input": {}} if action else {"type": "text", "text": ""}
            delta = {"type": "input_json_delta", "partial_json": json.dumps(action.arguments)} if action else {"type": "text_delta", "text": "finished"}
            event("content_block_start", {"type": "content_block_start", "index": 0, "content_block": block})
            event("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": delta})
            event("content_block_stop", {"type": "content_block_stop", "index": 0})
            event("message_delta", {"type": "message_delta", "delta": {"stop_reason": "tool_use" if action else "end_turn", "stop_sequence": None}, "usage": {"output_tokens": 1}})
            event("message_stop", {"type": "message_stop"})
        else:
            response = {"id": "resp_local_" + str(number), "object": "response", "created_at": 0, "status": "in_progress", "model": body.get("model"), "output": []}
            event("response.created", {"type": "response.created", "response": response})
            if action:
                names = {tool.get("name") for tool in body.get("tools", [])}
                if "exec_command" not in names:
                    raise ValueError("actual binary did not advertise exec_command")
                arguments = {"cmd": action.arguments["command"], "workdir": str(self.fixture.paths["A"]), "login": False, "yield_time_ms": 1000, "max_output_tokens": 1000}
                for key in ["workdir", "sandbox_permissions", "justification", "tty"]:
                    if key in action.arguments:
                        arguments[key] = action.arguments[key]
                item = {"type": "function_call", "id": "fc_local_" + str(number), "call_id": "call_local_" + str(number), "name": "exec_command", "arguments": json.dumps(arguments), "status": "completed"}
            else:
                item = {"type": "message", "id": "msg_local_final", "status": "completed", "role": "assistant", "content": [{"type": "output_text", "text": "finished", "annotations": []}]}
            event("response.output_item.added", {"type": "response.output_item.added", "output_index": 0, "item": item})
            event("response.output_item.done", {"type": "response.output_item.done", "output_index": 0, "item": item})
            event("response.completed", {"type": "response.completed", "response": {**response, "status": "completed", "output": [item], "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}})

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)


def command(binary, unavailable):
    argv = [str(binary)]
    if unavailable:
        if platform.system() == "Darwin":
            argv = ["/usr/bin/sandbox-exec", "-p", "(version 1)(allow default)(deny network-outbound)(allow network-outbound (remote ip \"localhost:*\"))(allow network-outbound (remote unix-socket))", *argv]
    return argv


def stop(process):
    if process.poll() is None:
        os.killpg(process.pid, signal.SIGTERM)
    try:
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait(timeout=3)


def tool_result(model, action_name):
    number = next(index + 1 for index, action in enumerate(model.actions) if action.name == action_name)
    for row in reversed(model.requests):
        body = row["body"]
        if model.protocol == "claude":
            for message in reversed(body.get("messages", [])):
                for item in message.get("content", []) if isinstance(message.get("content"), list) else []:
                    if item.get("type") == "tool_result" and item.get("tool_use_id") == "toolu_local_" + str(number):
                        return str(item.get("content", ""))
        else:
            for item in reversed(body.get("input", [])):
                if isinstance(item, dict) and item.get("type") == "function_call_output" and item.get("call_id") == "call_local_" + str(number):
                    return str(item.get("output", ""))
    return ""


def network_denied(fixture, model, unavailable):
    if any(row["path"] != "/network-control" for row in model.other_requests):
        return False
    if unavailable:
        return True
    result = tool_result(model, "socket-denied")
    if "PermissionError" in result and ("Operation not permitted" in result or "Permission denied" in result):
        return True
    if platform.system() != "Linux" or "ConnectionRefusedError: [Errno 111]" not in result:
        return False
    namespace = re.search(r"net:\[\d+\]", tool_result(model, "network-namespace"))
    return namespace is not None and namespace.group() != fixture.network_namespace


def save(fixture, model, binary, argv, result, process):
    result.update({
        "platform": platform.platform(),
        "binary_sha256": sha256(binary),
        "argv": argv,
        "environment": fixture.env,
        "scratch": str(fixture.root),
        "local_model_requests": len(model.requests),
        "model_request_routes": [row["path"] for row in model.requests],
        "offered_tools": model.requests[0]["body"].get("tools", []) if model.requests else [],
        "other_local_requests": model.other_requests,
        "model_errors": model.errors,
        "checkout_links": fixture.checkout_links,
        "preflight": fixture.preflight,
        "network_positive_control": fixture.network_control,
        "outer_network_namespace": fixture.network_namespace,
        "actions": [action.__dict__ for action in model.actions],
        "tool_results": {action.name: tool_result(model, action.name) for action in model.actions},
        "cleanup_observation": {
            "direct_child_exited": process.poll() is not None,
            "direct_child_returncode": process.returncode,
            "model_accept_thread_stopped": not model.thread.is_alive(),
            "model_listener_closed": model.server.socket.fileno() == -1,
            "descendant_cleanup_verified": False,
        },
    })
    (fixture.root / "requests.json").write_text(json.dumps(model.requests, indent=2) + "\n")
    (fixture.root / "receipt.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({"scratch": str(fixture.root), "checks": result["checks"], "requests": len(model.requests)}, indent=2), flush=True)
    return all(result["checks"].values())
