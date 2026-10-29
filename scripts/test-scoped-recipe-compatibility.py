#!/usr/bin/env python3
"""Fake-transport regressions for compatibility admission and observations."""

import importlib.util
import http.client
import io
import json
from pathlib import Path
import platform
import subprocess
import tempfile
import types
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location("compatibility", Path(__file__).with_name("verify-scoped-recipe-compatibility.py"))
proof = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(proof)


def observed_model(errors=(), accepted=1, attempted=1):
    tools = [{"name": name, "type": kind} for name, kind in proof.CODEX_TOOL_TYPES.items()]
    return types.SimpleNamespace(
        requests=[{"body": {"model": "gpt-5.5", "tools": tools}} for _ in range(accepted)],
        credentials=[{"all_present_credentials_are_explicit_dummy": True} for _ in range(attempted)],
        expected_posts=1, post_attempts=attempted, get_attempts=0, http_total=attempted,
        http_attempts=[{"method": "POST"} for _ in range(attempted)], http_overflow=0,
        other=[], errors=list(errors), action="text", endpoint="http://127.0.0.1:1/v1",
        close=lambda: None, thread=types.SimpleNamespace(is_alive=lambda: False),
        server=types.SimpleNamespace(socket=types.SimpleNamespace(fileno=lambda: -1)),
    )


class Observations(unittest.TestCase):
    def run_observed_case(self, model, case="codex"):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "fake-engine"
            binary.write_bytes(b"fake transport only")
            fixture = root / "fake-fixture"
            fixture.write_bytes(b"fake fixture only")
            args = types.SimpleNamespace(output=root, codex=str(binary), fixture=fixture, macos_sandbox_unavailable=False)
            value = {"env": [], "private": {"home": str(root)}, "model": "gpt-5.5", "session_id": "session"}
            policy = {"argv": [str(binary)], "env": [], "inventory": {"writable": []}, "unarmed": {"candidate_version": "fake"}}
            with mock.patch.dict(proof.PINS, {(platform.system(), platform.machine(), "codex"): proof.sha256(binary)}), \
                 mock.patch.object(proof, "LocalModel", return_value=model), \
                 mock.patch.object(proof, "obtain_codex_catalog", return_value=(root / "catalog", {"sha256": "0" * 64})), \
                 mock.patch.object(proof, "materialize_codex_profile", return_value={}), \
                 mock.patch.object(proof, "codex_profile_snapshot", return_value={}), \
                 mock.patch.object(proof, "codex_machine_preconditions", return_value={}), \
                 mock.patch.object(proof, "fixture_input", return_value=value), \
                 mock.patch.object(proof, "project", return_value=policy), \
                 mock.patch.object(proof, "inspect_writable_roots", return_value={"accepted": True}), \
                 mock.patch.object(proof, "inspect_selection", return_value={}), \
                 mock.patch.object(proof.platform, "platform", return_value="fake-platform"), \
                 mock.patch.object(proof, "native_command", side_effect=lambda argv, _: argv), \
                 mock.patch.object(proof.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "local-model-ready", "")), \
                 mock.patch.object(proof, "run_codex", return_value={"checks": {"task_completed": True}, "error": None, "direct_child_exited": True}), \
                 mock.patch("builtins.print"):
                return proof.run_case(args, case)

    def test_later_success_cannot_mask_server_error_or_rejected_attempt(self):
        for reason in ["unexpected credential", "unexpected route", "request bound exceeded"]:
            with self.subTest(reason=reason):
                result = self.run_observed_case(observed_model([reason], accepted=1, attempted=2))
                self.assertIsNotNone(result["error"])
                self.assertEqual(result["model_errors"], [reason])
                self.assertEqual(result["local_post_attempts"], 2)

    def test_exact_accepted_and_attempted_counts_are_required(self):
        for case, accepted, attempted in [("codex", 2, 2), ("codex", 1, 2), ("codex-declared-read", 1, 1)]:
            with self.subTest(case=case, accepted=accepted, attempted=attempted):
                result = self.run_observed_case(observed_model(accepted=accepted, attempted=attempted), case)
                self.assertIsNotNone(result["error"])

    def test_exact_clean_counts_pass(self):
        for case, count in [("codex", 1), ("codex-declared-read", 2)]:
            with self.subTest(case=case):
                result = self.run_observed_case(observed_model(accepted=count, attempted=count), case)
                self.assertIsNone(result["error"])
                self.assertTrue(all(result["checks"].values()))

    def test_shared_two_post_contract_also_rejects_extra_claude_attempt(self):
        self.assertTrue(all(proof.model_observation_checks(observed_model(accepted=2, attempted=2), 2).values()))
        self.assertFalse(all(proof.model_observation_checks(observed_model(accepted=2, attempted=3), 2).values()))

    def test_real_local_route_error_remains_fatal_after_success(self):
        model = proof.LocalModel("codex", Path("/synthetic/marker"), "text", False)
        connection = http.client.HTTPConnection("127.0.0.1", model.server.server_port, timeout=2)
        try:
            connection.request("GET", "/unexpected")
            response = connection.getresponse()
            self.assertEqual(response.status, 404)
            response.read()
            connection.request("POST", "/v1/responses", json.dumps({"model": "gpt-5.5"}))
            response = connection.getresponse()
            self.assertEqual(response.status, 200)
            response.read()
        finally:
            connection.close()
            model.close()
        self.assertEqual(len(model.requests), 1)
        self.assertEqual(model.post_attempts, 1)
        self.assertFalse(proof.model_observation_checks(model, 1)["model_server_error_free"])
        self.assertEqual(model.http_attempts[0]["path"], "/unexpected")

    def test_http_and_get_limits_keep_overflow_counts_and_first_bad_attempt(self):
        model = proof.LocalModel("codex", Path("/synthetic/marker"), "text", False)
        try:
            for index in range(proof.MAX_HTTP_REQUESTS + 2):
                allowed = model.record_http("GET", "/request-" + str(index), None)
                if index >= proof.MAX_GET_REQUESTS:
                    self.assertFalse(allowed)
            self.assertEqual(model.http_total, proof.MAX_HTTP_REQUESTS + 2)
            self.assertEqual(model.get_attempts, proof.MAX_HTTP_REQUESTS + 2)
            self.assertEqual(len(model.http_attempts), proof.MAX_HTTP_REQUESTS + 1)
            self.assertEqual(model.http_overflow, 1)
            self.assertEqual(model.http_attempts[proof.MAX_GET_REQUESTS]["path"], "/request-" + str(proof.MAX_GET_REQUESTS))
            self.assertFalse(proof.model_observation_checks(model, 1)["http_request_budget"])
        finally:
            model.close()

    def test_expected_websocket_rejection_has_a_valid_http11_envelope(self):
        model = proof.LocalModel("codex", Path("/synthetic/marker"), "text", False)
        connection = http.client.HTTPConnection("127.0.0.1", model.server.server_port, timeout=2)
        try:
            connection.request("GET", "/v1/responses", headers={"Upgrade": "websocket"})
            response = connection.getresponse()
            self.assertEqual((response.version, response.status), (11, 426))
            response.read()
            self.assertEqual((model.http_total, model.get_attempts, model.post_attempts), (1, 1, 0))
            self.assertEqual(model.errors, [])
        finally:
            connection.close()
            model.close()


class FakeProcess:
    def __init__(self, frames):
        self.stdin = io.StringIO()
        self.stdout = io.StringIO("".join(json.dumps(frame) + "\n" for frame in frames))
        self.returncode = None

    def poll(self):
        return self.returncode


class PrivateProfile(unittest.TestCase):
    def test_fixture_input_requires_a_catalog_for_codex_only(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(ValueError):
                proof.fixture_input(Path(directory), "codex", Path("/fake"), "http://127.0.0.1:1/v1")
            value = proof.fixture_input(Path(directory), "claude", Path("/fake"), "http://127.0.0.1:1/v1")
            self.assertNotIn("codex_catalog", value)

    def test_dummy_auth_and_content_addressed_catalog_are_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            source = root / "private-catalog.json"
            source.write_text('{"models":[]}\n')
            profile = root / "profile"
            profile.mkdir(mode=0o700)
            value = {"private": {"profile": str(profile)}, "codex_catalog": str(profile / "catalogs" / (proof.sha256(source) + ".json"))}
            before = proof.materialize_codex_profile(value, source)
            self.assertEqual(before, proof.codex_profile_snapshot(value))
            self.assertEqual(json.loads((profile / "auth.json").read_bytes()), {"auth_mode": "apikey", "OPENAI_API_KEY": proof.CODEX_DUMMY_CREDENTIAL})
            for name in ["config.toml", ".env", "rules", "profiles", "hooks.json"]:
                path = profile / name
                path.write_text("owned invalid-profile fixture\n")
                with self.assertRaises(ValueError):
                    proof.codex_profile_snapshot(value)
                path.unlink()
            (profile / "auth.json").write_text('{"auth_mode":"chatgpt","tokens":{"access_token":"owned-dummy"}}')
            with self.assertRaises(ValueError):
                proof.codex_profile_snapshot(value)

    def test_private_catalog_alias_or_changed_bytes_are_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            source, profile = root / "source", root / "profile"
            source.write_bytes(b"owned catalog bytes\n")
            profile.mkdir(mode=0o700)
            target = profile / "catalogs" / (proof.sha256(source) + ".json")
            value = {"private": {"profile": str(profile)}, "codex_catalog": str(target)}
            proof.materialize_codex_profile(value, source)
            target.write_bytes(b"changed\n")
            with self.assertRaises(ValueError):
                proof.codex_profile_snapshot(value)
            target.unlink()
            target.symlink_to(source)
            with self.assertRaises(ValueError):
                proof.codex_profile_snapshot(value)


class Dispatch(unittest.TestCase):
    def run_frames(self, ids, initial_task=False, premature_task=False, account=None):
        initial = {"kind": "write", "task": initial_task, "message": {"id": 1, "method": "initialize"}}
        account_request = {"kind": "write", "task": False, "message": {"id": 91, "method": "account/read", "params": {"refreshToken": False}}}
        opened = {"kind": "write", "task": False, "message": {"id": 2, "method": "thread/start"}}
        task = {"kind": "write", "task": True, "message": {"id": 3, "method": "turn/start"}}
        policy = {"cwd": "/synthetic", "protocol_actions": [initial]}
        responses = {
            1: {"id": 1, "result": {}},
            91: {"id": 91, "result": account if account is not None else {"account": {"type": "apiKey"}, "requiresOpenaiAuth": True}},
            90: {"id": 90, "result": {"data": [{"model": "gpt-5.5"}]}},
            2: {"id": 2, "result": {"model": "gpt-5.5", "modelProvider": "openai"}},
            3: {"id": 3, "result": {"turn": {"id": "turn"}}},
        }
        frames = [identity if isinstance(identity, dict) else responses[identity] for identity in ids] + [
            {"method": "turn/started", "params": {"turn": {"id": "turn"}}},
            {"method": "item/completed", "params": {"item": {"type": "agentMessage", "text": "finished"}}},
            {"method": "turn/completed", "params": {"turn": {"id": "turn", "status": "completed"}}},
        ]
        process = FakeProcess(frames)

        def generated(_, __, value):
            seen = {frame["id"] for frame in value["vendor_responses"]}
            actions = [initial]
            if 1 in seen:
                actions.extend([{"kind": "write", "task": False, "message": {"method": "initialized"}}, account_request])
            if 91 in seen:
                actions.append(opened)
            if 2 in seen:
                actions.append(task)
            if 3 in seen:
                actions.append({"kind": "started"})
            if premature_task and 2 not in seen:
                actions.append(task)
            return {**policy, "protocol_actions": actions}

        with tempfile.TemporaryDirectory() as directory, \
             mock.patch.object(proof.subprocess, "Popen", return_value=process), \
             mock.patch.object(proof, "project", side_effect=generated), \
             mock.patch.object(proof, "stop", side_effect=lambda child: setattr(child, "returncode", 0)):
            result = proof.run_codex(types.SimpleNamespace(fixture="fake"), Path(directory), ["fake"], {}, {}, policy, observed_model())
        sent = [json.loads(line) for line in process.stdin.getvalue().splitlines()]
        return result, sent

    def test_thread_response_before_catalog_cannot_forward_task(self):
        result, sent = self.run_frames([1, 91, 2, 90, 3])
        self.assertIsNotNone(result["error"])
        self.assertFalse(any(frame.get("method") == "turn/start" for frame in sent))

    def test_catalog_response_before_initialize_is_refused(self):
        result, sent = self.run_frames([90, 1, 91, 2, 3])
        self.assertIsNotNone(result["error"])
        self.assertFalse(any(frame.get("method") == "turn/start" for frame in sent))

    def test_normal_response_order_submits_one_task(self):
        result, sent = self.run_frames([1, 91, 90, 2, 3])
        self.assertIsNone(result["error"])
        self.assertTrue(all(result["checks"].values()))
        self.assertEqual(sum(frame.get("method") == "turn/start" for frame in sent), 1)

    def test_task_marked_initial_action_is_not_forwarded(self):
        result, sent = self.run_frames([1, 91, 90, 2, 3], initial_task=True)
        self.assertIsNotNone(result["error"])
        self.assertEqual(sent, [])

    def test_generated_task_before_thread_admission_is_not_forwarded(self):
        result, sent = self.run_frames([1, 91, 90, 2, 3], premature_task=True)
        self.assertIsNotNone(result["error"])
        self.assertFalse(any(frame.get("method") == "turn/start" for frame in sent))

    def test_account_catalog_and_task_order_is_exact(self):
        result, sent = self.run_frames([1, 91, 90, 2, 3])
        self.assertIsNone(result["error"])
        self.assertEqual([frame.get("id") for frame in sent], [1, None, 91, 90, 2, 3])
        self.assertEqual(sent[2]["params"], {"refreshToken": False})
        for ids in [[1, 90, 91, 2, 3], [1, 91, 91, 90, 2, 3], [1, 91, 2, 3]]:
            result, sent = self.run_frames(ids)
            self.assertIsNotNone(result["error"])
            self.assertFalse(any(frame.get("method") == "turn/start" for frame in sent))

    def test_signed_out_oauth_routing_and_non_api_accounts_are_refused(self):
        for account in [{"account": None, "requiresOpenaiAuth": True}, {"account": {"type": "chatgpt"}, "requiresOpenaiAuth": True}, {"account": {"type": "apiKey"}, "requiresOpenaiAuth": False}, {"account": {"type": "apiKey"}, "requiresOpenaiAuth": True, "workspaceRouting": {"id": "owned"}}]:
            result, sent = self.run_frames([1, 91, 90, 2, 3], account=account)
            self.assertIsNotNone(result["error"])
            self.assertFalse(any(frame.get("method") in {"thread/start", "turn/start", "model/list"} for frame in sent))

    def test_only_bounded_notifications_can_be_ignored(self):
        valid = {"method": "remoteControl/status/changed", "params": {"status": "disabled"}, "emittedAtMs": 1793303703845}
        result, _ = self.run_frames([1, valid, 91, 90, 2, 3])
        self.assertIsNone(result["error"])
        for invalid in [{**valid, "id": None}, {**valid, "id": 500}, {**valid, "result": {}}, {**valid, "error": None}, {**valid, "params": []}, {**valid, "emittedAtMs": True}, {**valid, "emittedAtMs": 2 ** 63}, {**valid, "emittedAtMs": "1"}, {**valid, "jsonrpc": "other"}]:
            result, sent = self.run_frames([1, invalid, 91, 90, 2, 3])
            self.assertIsNotNone(result["error"])
            self.assertFalse(any(frame.get("method") == "turn/start" for frame in sent))
        with mock.patch.object(proof, "MAX_NATIVE_FRAMES", 3):
            result, sent = self.run_frames([1, valid, valid, valid, 91, 90, 2, 3])
        self.assertIsNotNone(result["error"])
        self.assertFalse(any(frame.get("method") == "turn/start" for frame in sent))


if __name__ == "__main__":
    unittest.main()
