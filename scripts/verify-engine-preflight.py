#!/usr/bin/env python3
"""Check prototype admission refusal using scratch hardlinks and no engine execution."""

from pathlib import Path
from types import SimpleNamespace
import argparse
import importlib.util
import json
import os
import tempfile

import _engine_confinement as confinement
from _engine_confinement import inspect_writable_roots


def engine_modules():
    result = {}
    for engine in ["claude", "codex"]:
        source = Path(__file__).with_name("verify-" + engine + "-confinement.py")
        spec = importlib.util.spec_from_file_location("preflight_" + engine, source)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        result[engine] = module
    return result


def refusal_before_launch(module, fixture):
    counts = {"engine_launch_attempts": 0, "model_starts": 0}
    original = module.Fixture, module.Model, module.subprocess.run, module.subprocess.Popen

    def forbidden_engine(*_args, **_kwargs):
        counts["engine_launch_attempts"] += 1
        raise AssertionError("an engine launch was attempted before preflight refusal")

    def forbidden_model(*_args, **_kwargs):
        counts["model_starts"] += 1
        raise AssertionError("a local model service was started before preflight refusal")

    try:
        module.Fixture = lambda *_args: fixture
        module.Model = forbidden_model
        module.subprocess.run = forbidden_engine
        module.subprocess.Popen = forbidden_engine
        args = SimpleNamespace(output=str(fixture.root), inherited_hardlink=False, binary="not-executed")
        assert module.run(args) is False
        assert counts == {"engine_launch_attempts": 0, "model_starts": 0}
    finally:
        module.Fixture, module.Model, module.subprocess.run, module.subprocess.Popen = original
    return {**counts, "engine_spawns": 0, "model_contacts": 0}


def final_observation_timeout(root):
    root.mkdir(mode=0o700)
    elapsed, observations = [0], [0]
    original_stat, original_clock = Path.lstat, confinement.time.monotonic

    def delayed_observation(path, *args, **kwargs):
        metadata = original_stat(path, *args, **kwargs)
        if path == root:
            observations[0] += 1
            if observations[0] == 2:
                elapsed[0] = 2
        return metadata

    try:
        Path.lstat = delayed_observation
        confinement.time.monotonic = lambda: elapsed[0]
        inspection = inspect_writable_roots([root], timeout=1)
    finally:
        Path.lstat, confinement.time.monotonic = original_stat, original_clock
    assert observations[0] == 2
    inspection["clock_control"] = {"timeout_seconds": 1, "advanced_elapsed_seconds": 2, "root_observations": observations[0]}
    return inspection


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    output = Path(args.output).resolve()
    output.mkdir(mode=0o700, parents=True, exist_ok=True)
    destination = Path(tempfile.mkdtemp(prefix="preflight-", dir=output)).resolve()
    modules = engine_modules()
    records = []
    for location in [None, "A", "W", "O", "T"]:
        root = destination / (location or "positive")
        root.mkdir(mode=0o700)
        paths = {name: root / name for name in ["A", "B", "C", "W", "O", "T"]}
        for path in paths.values():
            path.mkdir(mode=0o700)
            (path / "seed").write_text("untouched\n")
        broad = list(paths.values())
        assert inspect_writable_roots(broad)["accepted"]
        narrowed = [paths[name] for name in ["A", "B", "W", "O", "T"]]
        if location is not None:
            alias = paths[location] / "retained-alias"
            if location == "A":
                alias = paths[location] / "nested" / "retained-alias"
                alias.parent.mkdir(mode=0o700)
            os.link(paths["C"] / "seed", alias)
        else:
            (paths["A"] / "unfollowed-symlink").symlink_to(paths["C"] / "absent")
        inspection = inspect_writable_roots(narrowed)
        assert inspection["accepted"] == (location is None)
        launches = {}
        if location is not None:
            assert "multiple hardlinks" in inspection["reason"]
            fixture = SimpleNamespace(root=root, roots=narrowed)
            launches = {engine: refusal_before_launch(module, fixture) for engine, module in modules.items()}
        assert (paths["C"] / "seed").read_text() == "untouched\n"
        records.append({"case": location or "positive-no-hardlinks", "inspection": inspection, "launches": launches, "outside_contents_unchanged": True})

    fixture = destination / "positive"
    missing = inspect_writable_roots([fixture / "absent"])
    assert not missing["accepted"]
    bounded = inspect_writable_roots([fixture / "A"], max_entries=0)
    assert not bounded["accepted"]
    unreadable = fixture / "unreadable"
    unreadable.mkdir(mode=0o700)
    unreadable.chmod(0)
    try:
        inaccessible = inspect_writable_roots([unreadable])
        assert not inaccessible["accepted"]
    finally:
        unreadable.chmod(0o700)
    expired = final_observation_timeout(destination / "deadline")
    print(json.dumps({"final_observation_timeout": expired}), flush=True)
    assert not expired["accepted"]
    receipt = {"prototype_only": True, "cases": records, "missing_root": missing, "bounded_inspection": bounded, "unreadable_directory": inaccessible, "final_observation_timeout": expired, "engine_spawns": 0, "model_contacts": 0}
    (destination / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps({"receipt": str(destination / "receipt.json"), "passed_cases": 9, "engine_spawns": 0, "model_contacts": 0}, indent=2), flush=True)


if __name__ == "__main__":
    main()
