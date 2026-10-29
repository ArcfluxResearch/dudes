#!/usr/bin/env python3
"""Fast fake-engine controls for generated confinement evidence admission."""

import importlib.util
import io
import json
from pathlib import Path
import platform
import subprocess
import sys
import tempfile
import types
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location("generated_confinement", Path(__file__).with_name("verify-generated-scoped-confinement.py"))
proof = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = proof
SPEC.loader.exec_module(proof)


def requests_for(provider, probe, text, *, call=True, output=True, tool_type=None):
    if provider == "claude":
        messages = []
        if call:
            messages.append({"role": "assistant", "content": [{"type": "tool_use", "id": proof.CLAUDE_CALL_ID, "name": probe.tool, "input": probe.arguments}]})
        if output:
            messages.append({"role": "user", "content": [{"type": "tool_result", "tool_use_id": proof.CLAUDE_CALL_ID, "content": text, "is_error": not probe.targets[0].writable}]})
        body = {"messages": messages}
    else:
        items = []
        if call:
            items.append({"type": tool_type or ("custom_tool_call" if probe.tool == "apply_patch" else "function_call"), "call_id": proof.CALL_ID, "name": probe.tool, **({"input": probe.arguments} if probe.tool == "apply_patch" else {"arguments": json.dumps(probe.arguments)})})
        if output:
            items.append({"type": "custom_tool_call_output" if probe.tool == "apply_patch" else "function_call_output", "call_id": proof.CALL_ID, "output": text})
        body = {"input": items}
    return [{"body": {}}, {"body": body}]


class Evidence(unittest.TestCase):
    def test_unchanged_negative_requires_actual_refusal_and_corresponding_call(self):
        with tempfile.TemporaryDirectory() as directory:
            target = proof.Target("undeclared", Path(directory) / "target", False)
            target.path.write_text("before\n")
            for provider, tool in [("claude", "Edit"), ("codex", "apply_patch")]:
                probe = proof.Probe("file-undeclared", tool, {} if provider == "claude" else "patch", [target])
                for text, call, output in [("", True, True), ("File has not been read yet", True, True), ("permission denied", False, True), ("permission denied", True, False)]:
                    with self.subTest(provider=provider, text=text, call=call, output=output):
                        checks, _, _ = proof.effect_checks(provider, probe, {target.name: b"before\n"}, requests_for(provider, probe, text, call=call, output=output))
                        self.assertFalse(all(checks.values()))
                checks, _, _ = proof.effect_checks(provider, probe, {target.name: b"before\n"}, requests_for(provider, probe, "Permission denied"))
                self.assertTrue(all(checks.values()))

    def test_success_or_refusal_cannot_mask_wrong_target_effect(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "target"
            for writable, before, after, text in [(True, None, None, "Success. Updated"), (False, b"before\n", proof.MARKER.encode(), "permission denied")]:
                if after is not None:
                    path.write_bytes(after)
                target = proof.Target("target", path, writable)
                probe = proof.Probe("file", "apply_patch", "patch", [target])
                checks, _, _ = proof.effect_checks("codex", probe, {"target": before}, requests_for("codex", probe, text))
                self.assertFalse(checks["target_effect"])

    def test_custom_patch_requires_custom_native_call_type(self):
        with tempfile.TemporaryDirectory() as directory:
            target = proof.Target("target", Path(directory) / "target", True)
            target.path.write_text(proof.MARKER)
            probe = proof.Probe("file", "apply_patch", "patch", [target])
            for kind in ["function_call", "custom_tool_call"]:
                checks, _, _ = proof.effect_checks("codex", probe, {"target": None}, requests_for("codex", probe, "Success. Updated", tool_type=kind))
                self.assertEqual(all(checks.values()), kind == "custom_tool_call")

    def test_grouped_command_requires_each_path_observation(self):
        with tempfile.TemporaryDirectory() as directory:
            targets = [proof.Target(name, Path(directory) / name, True) for name in ["a", "b"]]
            for target in targets:
                target.path.write_text(proof.MARKER)
            probe = proof.Probe("command-writable", "exec_command", {"cmd": "fake"}, targets)
            checks, _, _ = proof.effect_checks("codex", probe, {"a": None, "b": None}, requests_for("codex", probe, "SCOPED_a=written"))
            self.assertTrue(checks["a_tool_observed"])
            self.assertFalse(checks["b_tool_observed"])

    def test_existing_file_edit_requires_the_explicit_read_result(self):
        with tempfile.TemporaryDirectory() as directory:
            target = proof.Target("target", Path(directory) / "target", False)
            target.path.write_text("before\n")
            probe = proof.Probe("file", "Edit", {"file_path": str(target.path)}, [target], preread=True)
            requests = requests_for("claude", probe, "denied by your permission settings")
            checks, _, _ = proof.effect_checks("claude", probe, {"target": b"before\n"}, requests)
            self.assertFalse(checks["explicit_prerequisite_read_observed"])
            requests[-1]["body"]["messages"].extend([
                {"role": "assistant", "content": [{"type": "tool_use", "id": proof.CLAUDE_READ_ID, "name": "Read", "input": {"file_path": str(target.path)}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": proof.CLAUDE_READ_ID, "content": "1: before"}]},
            ])
            checks, _, _ = proof.effect_checks("claude", probe, {"target": b"before\n"}, requests)
            self.assertTrue(all(checks.values()))

    def test_composite_refusal_requires_restricted_read_then_unread_edit_and_exact_effects(self):
        with tempfile.TemporaryDirectory() as directory:
            target = proof.Target("target", Path(directory) / "target", False)
            target.path.write_text("before\n")
            probe = proof.Probe("file-undeclared", "Edit", {"file_path": str(target.path)}, [target], preread=True)
            for failure in [None, "missing read", "missing edit", "successful read", "wrong id", "wrong role", "changed target", "extra attempt"]:
                target.path.write_text("changed\n" if failure == "changed target" else "before\n")
                requests = requests_for("claude", probe, "File has not been read yet")
                if failure == "missing edit":
                    requests[-1]["body"]["messages"].pop(0)
                identifier = "wrong-read" if failure == "wrong id" else proof.CLAUDE_READ_ID
                if failure != "missing read":
                    requests[-1]["body"]["messages"].extend([
                        {"role": "user" if failure == "wrong role" else "assistant", "content": [{"type": "tool_use", "id": identifier, "name": "Read", "input": {"file_path": str(target.path)}}]},
                        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": identifier, "content": "before" if failure == "successful read" else "--restricted confines the file tools to the working directory", "is_error": failure != "successful read"}]},
                    ])
                requests.insert(1, {"body": {}})
                checks, outputs, _ = proof.effect_checks("claude", probe, {"target": b"before\n"}, requests)
                checks.update(proof.compat.model_observation_checks(types.SimpleNamespace(errors=[], requests=requests, post_attempts=4 if failure == "extra attempt" else 3, http_total=3, get_attempts=0, http_overflow=0), 3))
                self.assertEqual(all(checks.values()), failure is None, failure)
                observation = proof.read_then_edit_observation(probe, {"target": b"before\n"}, requests, outputs)
                self.assertFalse(observation["edit_guard_observed"])

    def test_unavailable_requires_actual_sandbox_error(self):
        with tempfile.TemporaryDirectory() as directory:
            target = proof.Target("target", Path(directory) / "target", False)
            probe = proof.Probe("command-writable", "Bash", {}, [target], unavailable=True)
            for text, allowed in [("permission denied", False), ("sandbox_apply: Operation not permitted", True)]:
                checks, _, _ = proof.effect_checks("claude", probe, {"target": None}, requests_for("claude", probe, text))
                self.assertEqual(all(checks.values()), allowed)

    def test_network_connection_refused_requires_distinct_linux_namespace(self):
        probe = proof.Probe("command-network", "exec_command", {"cmd": "fake"}, [], network=True)
        with mock.patch.object(proof.platform, "system", return_value="Linux"):
            for inner, outer, passed in [("net:[1]", "net:[1]", False), ("net:[2]", "net:[1]", True), ("net:[2]", None, False)]:
                text = "SCOPED_NETWORK_ERRNO=" + str(proof.errno.ECONNREFUSED) + "\nSCOPED_NETNS=" + inner
                checks, _, _ = proof.effect_checks("codex", probe, {}, requests_for("codex", probe, text), outer)
                self.assertEqual(all(checks.values()), passed)


class Transport(unittest.TestCase):
    def test_narrow_unavailable_readiness_retains_first_failure_and_requires_owned_denial(self):
        endpoint = types.SimpleNamespace(endpoint="http://127.0.0.1:12345/v1")
        other = types.SimpleNamespace(endpoint="http://127.0.0.1:12346/v1", http_total=0, errors=[], http_attempts=[])
        guard = proof.unavailable_guard(endpoint.endpoint)
        self.assertIn('localhost:12345', guard)
        self.assertNotIn('localhost:*', guard)
        for q_exit, expected in [(7, True), (0, False)]:
            responses = [subprocess.CompletedProcess([], 7, "", "first local connection failed"), subprocess.CompletedProcess([], 0, "local-model-ready", ""), subprocess.CompletedProcess([], q_exit, "" if q_exit else "local-model-ready", ""), subprocess.CompletedProcess([], q_exit, "" if q_exit else "local-model-ready", "")]
            with mock.patch.object(proof.subprocess, "run", side_effect=responses):
                result = proof.unavailable_prerequisites(endpoint, other, {}, guard)
            self.assertEqual(all(result["checks"].values()), expected)
            self.assertEqual(len(result["attempts"]), 4)
            self.assertEqual(result["attempts"][0]["exit"], 7)
            self.assertEqual(result["attempts"][1]["stdout"], "local-model-ready")

    def test_server_sends_exact_custom_patch_then_final_and_retains_all_attempts(self):
        model = proof.ActionModel("codex", False)
        model.probe = proof.Probe("file", "apply_patch", "*** Begin Patch\n*** End Patch", [])
        handler = types.SimpleNamespace(wfile=io.BytesIO())
        body = {"model": "gpt-5.5", "tools": [{"name": name, "type": kind} for name, kind in proof.compat.CODEX_TOOL_TYPES.items()]}
        try:
            for index in [1, 2]:
                self.assertTrue(model.record_http("POST", "/v1/responses", None))
                model.requests.append({"body": body})
                model.respond(handler, body)
            wire = handler.wfile.getvalue().decode()
            self.assertIn('"type": "custom_tool_call"', wire)
            self.assertIn('"input": "*** Begin Patch\\n*** End Patch"', wire)
            self.assertEqual(wire.count('"type": "custom_tool_call",'), 3)
            self.assertTrue(all(proof.compat.model_observation_checks(model, 2).values()))
            self.assertFalse(model.record_http("POST", "/v1/responses", None))
            self.assertFalse(all(proof.compat.model_observation_checks(model, 2).values()))
        finally:
            model.close()

    def test_request_inventory_and_model_are_checked_before_action(self):
        model = proof.ActionModel("codex", False)
        model.probe = proof.Probe("file", "apply_patch", "patch", [])
        handler = types.SimpleNamespace(wfile=io.BytesIO())
        try:
            model.requests.append({"body": {}})
            for body in [{"model": "gpt-5.5", "tools": []}, {"model": "other", "tools": [{"name": name, "type": kind} for name, kind in proof.compat.CODEX_TOOL_TYPES.items()]}]:
                with self.assertRaises(ValueError):
                    model.respond(handler, body)
            self.assertEqual(handler.wfile.getvalue(), b"")
        finally:
            model.close()


class Fixtures(unittest.TestCase):
    def test_linux_bridge_budget_covers_both_socket_names_and_the_nul(self):
        for temp, fits in [("/tmp/dp-owned/agent/tmp", True), ("/tmp/" + "x" * 100, False)]:
            result = proof.linux_bridge_path_budget({"private": {"temp": temp}})
            self.assertEqual(result["http_path_bytes"], len(temp.encode()) + 34)
            self.assertEqual(result["socks_path_bytes"], len(temp.encode()) + 35)
            self.assertEqual(result["maximum_with_nul_bytes"], len(temp.encode()) + 36)
            self.assertEqual(result["fits"], fits)

    def test_existing_negative_targets_and_seed_identity_are_rechecked(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            value = proof.compat.fixture_input(root, "codex", Path("/fake-engine"), "http://127.0.0.1:1/v1")
            traps = proof.seed_canaries(root, value, "codex")
            probe = proof.make_probe(value, "codex", "file-undeclared", "http://127.0.0.1:1/v1")
            self.assertTrue(probe.targets[0].path.is_file())
            creation = proof.make_probe(value, "claude", "file-undeclared-create", "http://127.0.0.1:1/v1")
            declared = proof.make_probe(value, "claude", "file-declared", "http://127.0.0.1:1/v1")
            self.assertTrue(creation.targets[0].path.parent.is_dir())
            self.assertFalse(creation.targets[0].path.exists())
            self.assertEqual(creation.arguments["old_string"], declared.arguments["old_string"])
            self.assertEqual(creation.arguments["new_string"], declared.arguments["new_string"])
            self.assertFalse(any(path.exists() for path in traps))
            state = proof.owned_seed_state(value)
            self.assertEqual(state, proof.owned_seed_state(value))
            config = Path(value["private"]["profile"]) / "config.toml"
            config.write_text(config.read_text() + "\n# changed\n")
            self.assertNotEqual(state, proof.owned_seed_state(value))

    def test_successful_native_return_cannot_mask_failed_transport_or_missing_action(self):
        original_run = subprocess.run
        for failure in [None, "handler error", "extra attempt", "missing action", "health refused"]:
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                binary, fixture = root / "fake-engine", root / "fake-fixture"
                binary.write_bytes(b"fake engine, never launched")
                fixture.write_bytes(b"fake fixture, never launched")
                model = types.SimpleNamespace(requests=[], errors=[], post_attempts=0, get_attempts=1, http_total=1, http_attempts=[], http_overflow=0, credentials=[], endpoint="http://127.0.0.1:1/v1", close=lambda: None, thread=types.SimpleNamespace(is_alive=lambda: False), server=types.SimpleNamespace(server_port=1, socket=types.SimpleNamespace(fileno=lambda: -1)))
                args = types.SimpleNamespace(output=root, fixture=fixture, codex=str(binary), macos_sandbox_unavailable=False)
                def fake_project(_, __, value):
                    cwd = value["grant"]["repositories"]["entries"][0]["value"]["execution"]["directory"]
                    return {"argv": [str(binary)], "env": [], "cwd": cwd, "unarmed": {"candidate_version": "fake"}, "inventory": {"writable": [{"path": value["private"]["work"]}]}}
                def fake_run(argv, **kwargs):
                    if argv[0] == "/usr/bin/git":
                        return original_run(argv, **kwargs)
                    if argv[0] == "/usr/bin/curl" and failure == "health refused":
                        return subprocess.CompletedProcess(argv, 7, "", "local connection failed")
                    return subprocess.CompletedProcess(argv, 0, "codex-cli 0.164.0-alpha.11.1" if argv[-1] == "--version" else "local-model-ready", "")
                def fake_native(*call_args):
                    probe = call_args[-1].probe
                    probe.targets[0].path.write_text(proof.MARKER)
                    model.requests = requests_for("codex", probe, "Success. Updated", call=failure != "missing action")
                    model.post_attempts = 2
                    model.http_total = 3
                    if failure == "handler error":
                        model.errors.append("owned handler failed")
                    if failure == "extra attempt":
                        model.post_attempts = 3
                    return {"checks": {"fake_native_completed": True}, "error": None, "direct_child_exited": True}
                with mock.patch.dict(proof.compat.PINS, {(platform.system(), platform.machine(), "codex"): proof.sha256(binary)}), \
                     mock.patch.object(proof, "ActionModel", return_value=model), \
                     mock.patch.object(proof.compat, "project", side_effect=fake_project), \
                     mock.patch.object(proof.compat, "inspect_selection", return_value={"profile_initially_empty": True}), \
                     mock.patch.object(proof.compat, "native_command", side_effect=lambda argv, _: argv), \
                     mock.patch.object(proof.compat, "run_codex", side_effect=fake_native), \
                     mock.patch.object(proof.platform, "platform", return_value="fake native control"), \
                     mock.patch.object(proof.subprocess, "run", side_effect=fake_run), \
                     mock.patch("builtins.print"):
                    result = proof.run_case(args, "codex", "file-declared")
                self.assertEqual(result["passed"], failure is None)
                self.assertEqual(result["local_post_attempts"], 0 if failure == "health refused" else 3 if failure == "extra attempt" else 2)
                if failure == "health refused":
                    self.assertEqual(result["local_endpoint_positive_control"]["exit"], 7)
                    self.assertFalse(result["checks"]["owned_endpoint_positive_control"])
                    self.assertIsNone(result["cleanup_observation"]["direct_child_exited"])
                self.assertTrue(Path(result["scratch"], "receipt.json").is_file())


class Resume(unittest.TestCase):
    def test_native_stage_requires_the_actual_init_and_result_session_ids(self):
        for missing in [None, "init", "result"]:
            with self.subTest(missing=missing), tempfile.TemporaryDirectory() as directory:
                initial = {"type": "system", "subtype": "init", "session_id": "session", "tools": ["Bash", "Read", "Edit"], "mcp_servers": [], "permissionMode": "dontAsk", "model": "claude-sonnet-4-6"}
                result = {"type": "result", "subtype": "success", "session_id": "session", "result": "finished", "is_error": False}
                if missing:
                    (initial if missing == "init" else result).pop("session_id")
                rows = [initial, {"type": "assistant", "session_id": "session", "message": {"content": [{"type": "tool_use", "id": "read"}]}}, result]
                process = types.SimpleNamespace(pid=321, returncode=0, communicate=lambda *_args, **_kwargs: ("\n".join(json.dumps(row) for row in rows), ""), poll=lambda: 0)
                with mock.patch.object(proof.subprocess, "Popen", return_value=process), mock.patch.object(proof.os, "getpgid", return_value=321), mock.patch.object(proof, "stop"):
                    observed = proof.run_claude(Path(directory), ["fake"], {}, {"task": "one task", "session_id": "session", "model": "claude-sonnet-4-6"}, {"cwd": directory}, None, ["read"])
                self.assertEqual(all(observed["checks"].values()), missing is None)

    @staticmethod
    def synthetic_input(root, provider, binary, endpoint):
        entries = []
        for name in ["a", "b", "c"]:
            copy, origin = root / "copies" / name, root / "origins" / name
            gitdir = origin / ".git" / "worktrees" / name
            copy.mkdir(parents=True)
            gitdir.mkdir(parents=True)
            (copy / ".git").write_text("gitdir: " + str(gitdir) + "\n")
            entries.append({"repo_id": name, "value": {"execution": {"directory": str(copy), "git_dir": str(gitdir)}, "origin": {"directory": str(origin), "common_dir": str(origin / ".git")}}})
        private = {"home": str(root / "home"), "profile": str(root / "profile"), **{name: str(root / "agent" / directory) for name, directory in [("work", "work"), ("outbox", "outbox"), ("temp", "tmp")]}}
        for path in [*private.values(), str(root / "agent" / "inbox")]:
            Path(path).mkdir(parents=True)
        env = {"HOME": private["home"], "CLAUDE_CONFIG_DIR": private["profile"], "ANTHROPIC_API_KEY": proof.compat.DUMMY_CREDENTIAL, "ANTHROPIC_BASE_URL": endpoint.removesuffix("/v1"), "PATH": "/usr/bin:/bin"}
        return {"private": private, "env": list(env.items()), "hired": {"repositories": {"primary": "a", "entries": entries}}, "grant": {"repositories": {"primary": "b", "entries": [entries[1], entries[0]]}}, "legacy_cwd": entries[0]["value"]["execution"]["directory"], "agent_dir": str(root / "agent"), "session_id": "owned-session", "session_kind": "first", "model": "claude-sonnet-4-6", "task": "synthetic"}

    def test_actual_two_stage_checker_requires_identity_grant_actions_and_exact_counts(self):
        for failure in [None, "group present", "first canary", "wide grant", "wrong session", "extra attempt", "wrong action", "unread file"]:
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                binary, fixture = root / "fake-engine", root / "fake-fixture"
                binary.write_bytes(b"never launched")
                fixture.write_bytes(b"never launched")
                models, calls, prior_messages, execution_roots = [], [], [], []
                def model_factory(*_):
                    port = 12345 + len(models)
                    model = types.SimpleNamespace(endpoint="http://127.0.0.1:" + str(port) + "/v1", requests=[], errors=[], credentials=[], post_attempts=0, get_attempts=0, http_total=0, http_attempts=[], http_overflow=0, close=lambda: None, thread=types.SimpleNamespace(is_alive=lambda: False), server=types.SimpleNamespace(server_port=port, socket=types.SimpleNamespace(fileno=lambda: -1)))
                    models.append(model)
                    return model
                def project(_, __, value):
                    roots = [entry["value"]["execution"]["directory"] for entry in value["grant"]["repositories"]["entries"]] + [value["private"][name] for name in ["work", "outbox", "temp"]]
                    if failure == "wide grant" and value["session_kind"] == "resume":
                        roots.append(value["legacy_cwd"])
                    protected = [str(target.path) for target in proof.protected_targets({**value, "grant": {"repositories": {"entries": [value["hired"]["repositories"]["entries"][1], value["hired"]["repositories"]["entries"][0]]}}})]
                    settings = {"permissions": {"allow": roots}, "sandbox": {"filesystem": {"allowWrite": roots, "denyWrite": protected}}}
                    argv = [str(binary.resolve()), "--settings", json.dumps(settings), "--session-id" if value["session_kind"] == "first" else "--resume", value["session_id"]]
                    return {"argv": argv, "cwd": roots[0], "env": [{"name": name, "source": index} for index, (name, _) in enumerate(value["env"])], "inventory": {"writable": [{"path": path} for path in roots], "protected": protected}}
                def native(stage_root, argv, env, value, policy, probe, expected_ids):
                    model = models[-1]
                    calls.append(value["session_kind"])
                    execution_roots.append(Path(value["agent_dir"]).parent)
                    messages = [*prior_messages, {"role": "user", "content": value["task"]}]
                    tools = [{"name": name} for name in ["Bash", "Read", "Edit"]]
                    def request():
                        return {"body": {"model": value["model"], "tools": tools, "messages": json.loads(json.dumps(messages))}}
                    model.requests = [request()]
                    for action in model.plan:
                        identifier = "unexpected-action" if failure == "wrong action" and action.tool == "Bash" else action.id
                        denied = action.id == "toolu_resume_old_edit"
                        if action.tool == "Read":
                            text = Path(action.arguments["file_path"]).read_text()
                        elif denied:
                            text = "File has not been read yet" if failure == "unread file" else "denied by your permission settings"
                        elif action.tool == "Edit":
                            Path(action.arguments["file_path"]).write_text(proof.MARKER)
                            text = "Success. Updated"
                        else:
                            (Path(policy["cwd"]) / "resume-shell").write_text(proof.MARKER)
                            text = "SCOPED_removed-a=refused\nSCOPED_declared-b=written\n"
                        messages.extend([{"role": "assistant", "content": [{"type": "tool_use", "id": identifier, "name": action.tool, "input": action.arguments}]}, {"role": "user", "content": [{"type": "tool_result", "tool_use_id": identifier, "content": text, **({"is_error": True} if denied else {})}]}])
                        model.requests.append(request())
                    model.post_attempts = len(model.requests) + (1 if failure == "extra attempt" and value["session_kind"] == "resume" else 0)
                    model.http_total = model.post_attempts
                    artifact = Path(value["private"]["profile"]) / "projects" / (value["session_id"] + ".jsonl")
                    artifact.parent.mkdir(exist_ok=True)
                    if value["session_kind"] == "first":
                        artifact.write_text("")
                    with artifact.open("a") as output:
                        for message in messages[len(prior_messages):]:
                            output.write(json.dumps({"sessionId": value["session_id"], "message": message}) + "\n")
                    prior_messages[:] = messages
                    if failure == "first canary" and value["session_kind"] == "first":
                        (Path(value["agent_dir"]).parent / "hook-ran").touch()
                    return {"checks": {"task_completed": True, "recorded_session_exact": not (failure == "wrong session" and value["session_kind"] == "resume")}, "error": None, "native_initializations": [{"claude_code_version": "2.1.311"}], "leader_reaped": True, "owned_process_group": 123, "native_leader_pid": 123, "direct_child_exited": True}
                args = types.SimpleNamespace(output=root, claude=str(binary), fixture=fixture)
                with mock.patch.object(proof.platform, "system", return_value="Linux"), \
                     mock.patch.dict(proof.compat.PINS, {(platform.system(), platform.machine(), "claude"): proof.sha256(binary)}), \
                     mock.patch.object(proof, "ActionModel", side_effect=model_factory), \
                     mock.patch.object(proof.compat, "fixture_input", side_effect=self.synthetic_input), \
                     mock.patch.object(proof.compat, "project", side_effect=project), \
                     mock.patch.object(proof.compat, "inspect_selection", return_value={"profile_initially_empty": True}), \
                     mock.patch.object(proof.compat, "native_command", side_effect=lambda argv, _: argv), \
                     mock.patch.object(proof, "run_claude", side_effect=native), \
                     mock.patch.object(proof, "owned_group_absence", return_value={"absent": failure != "group present"}), \
                     mock.patch("builtins.print"):
                    result = proof.run_claude_resume(args)
                self.assertEqual(result["passed"], failure is None, {"error": result["error"], "checks": result["checks"]})
                self.assertEqual(calls, ["first"] if failure in {"group present", "first canary", "wide grant"} else ["first", "resume"])
                self.assertNotEqual(Path(result["execution_root"]), Path(result["scratch"]))
                self.assertEqual(execution_roots, [Path(result["execution_root"])] * len(calls))
                self.assertTrue(Path(result["execution_root"]).is_dir())
                self.assertTrue(result["linux_bridge_path_budget"]["fits"])
                if failure is None:
                    self.assertEqual([stage["local_post_attempts"] for stage in result["stages"]], [2, 4])
                    self.assertFalse(result["stages"][1]["pre_launch_observations"]["initial_empty_claim_used"])


if __name__ == "__main__":
    unittest.main()
