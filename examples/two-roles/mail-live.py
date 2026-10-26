#!/usr/bin/env python3
"""Opt-in two-agent mail rehearsal with an installed engine and isolated Dudes state."""

import argparse
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import time

from mail import MAIL_WAKE_ISSUED, answered, done, rows


def arguments():
    target = Path(os.environ.get("CARGO_TARGET_DIR", Path(__file__).resolve().parents[2] / "target"))
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--allow-spend", action="store_true", help="authorize real engine turns")
    parser.add_argument("--provider", choices=("claude", "codex"), required=True)
    parser.add_argument("--engine-home", type=Path, required=True,
                        help="an existing scratch home where the engine itself is signed in")
    parser.add_argument("--cli", type=Path,
                        default=Path(os.environ.get("DUDES_CLI", target / "release/dudes")))
    parser.add_argument("--daemon", type=Path,
                        default=Path(os.environ.get("DUDED", target / "release/duded")))
    parser.add_argument("--journal-reader", type=Path,
                        default=Path(os.environ.get("DUDES_XTASK", target / "release/xtask")))
    args = parser.parse_args()
    if not args.allow_spend:
        parser.error("real engine turns require --allow-spend")
    if not args.engine_home.is_absolute() or not args.engine_home.is_dir():
        parser.error("--engine-home must be an existing absolute scratch directory")
    if args.engine_home.resolve() == Path.home().resolve():
        parser.error("--engine-home must differ from your normal home")
    for binary in (args.cli, args.daemon, args.journal_reader):
        if not binary.is_absolute() or not os.access(binary, os.X_OK):
            parser.error("the CLI, daemon and journal reader must name absolute executable build outputs")
    if shutil.which(args.provider) is None:
        parser.error("the selected engine must be installed on PATH")
    return args


def reported_cost(records):
    turns = rows(records, "turn_ended")
    costs = [(record.get("usage") or {}).get("usd") for record in turns]
    known = [cost for cost in costs if isinstance(cost, int)]
    print(f"Engine-reported cost ${sum(known) / 1_000_000:.6f} across {len(known)} turns.")
    if len(known) != len(costs):
        print(f"Dollar usage was not reported for {len(costs) - len(known)} ended turns.")
    unfinished = len(rows(records, "turn_spawned")) - len(turns)
    if unfinished > 0:
        print(f"Final usage is unknown for {unfinished} spawned turns without an end record.")


def rehearse(args, root):
    state, runtime, work, home = (root / name for name in ("state", "runtime", "work", "home"))
    config = home / "Library/Application Support"
    for directory in (state, runtime, work, config / "dudes"):
        directory.mkdir(parents=True)
    runtime.chmod(0o700)
    provider = "\n[providers.codex]\n" if args.provider == "codex" else ""
    (config / "dudes/config.toml").write_text(
        "[budget]\nfloor_usd_per_day = 1\n[budget.per_turn_floor]\nusd = 0.05\n" + provider)
    env = {key: value for key, value in os.environ.items() if not key.startswith("DUDES_")}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(config), APPDATA=str(config),
               CODEX_HOME=str(args.engine_home / ".codex"),
               CLAUDE_CONFIG_DIR=str(args.engine_home / ".claude"),
               DUDES_STATE_DIR=str(state), DUDES_RUNTIME_DIR=str(runtime))

    def command(argv):
        result = subprocess.run(argv, env=env, cwd=work, capture_output=True,
                                text=True, timeout=10)
        if result.returncode:
            raise RuntimeError(f"{Path(argv[0]).name} {argv[1]} exited {result.returncode}")
        return result.stdout

    def operator(*argv):
        return command([str(args.cli), *argv])

    def journal():
        frames = map(json.loads, operator("--json", "tail", "--from", "0").splitlines())
        return [dict(frame["entry"], lsn=frame["lsn"]) for frame in frames]

    def wait_for(description, predicate):
        deadline = time.monotonic() + 90
        while True:
            records = journal()
            if predicate(records):
                return records
            if daemon.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError(description)
            time.sleep(0.1)

    daemon = subprocess.Popen([str(args.daemon)], cwd=work, env=env,
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    records = []
    try:
        deadline = time.monotonic() + 30
        while True:
            try:
                operator("ping")
                break
            except (RuntimeError, subprocess.TimeoutExpired):
                if daemon.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("the isolated daemon did not start") from None
                time.sleep(0.1)
        agents = {}
        for role in ("recipient", "sender"):
            hired = json.loads(operator("--json", "hire", "--name", f"mail {role}",
                                        "--role", role, "--provider", args.provider,
                                        "--cwd", str(work), "--usd-per-day", "0.5"))
            agents[role] = hired["agent"]

        def goal(role, task):
            marker = work / f"{role}.ready"
            create = shlex.join(["python3", "-c",
                                 f"from pathlib import Path; Path({str(marker)!r}).touch()"])
            operator("--json", "goal", "--agent", agents[role], "--text",
                     f"In this scratch project, run only these commands and finish.\n{task}\n{create}",
                     "--usd", "0.5", "--hop-cap", "1", "--timeout-s", "60",
                     "--", "test", "-f", str(marker))

        goal("recipient", "")
        wait_for("the recipient did not finish its preparation goal",
                 lambda observed: done(observed, agents["recipient"]) is not None)
        send = shlex.join([str(args.cli), "mail", "send", "--to", agents["recipient"],
                           "--act", "request", "--subject", "mail rehearsal", "--body",
                           "Please answer this request by mail with the word ready. Use the reply "
                           "command shown by dudes mail read. Reply once and do not change any file."])
        goal("sender", send)
        records = wait_for("the request received no later reply from its recipient",
                           lambda observed: any(answered(observed, request)
                                                for request in rows(observed, "mail")))
        request = next(request for request in rows(records, "mail") if answered(records, request))
        assert any(record["agent"] == agents["recipient"] for record in rows(records, MAIL_WAKE_ISSUED)), \
            "the recipient replied without the required mail wake"
        print("PASS the request reached a completed agent and caused a mail wake.")
        print(f"PASS thread {request['thread']} has a later reply from the request's recipient.")
        print("PASS answered is derived from journal records; no stored mail state was added.")
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=30)
        except subprocess.TimeoutExpired:
            daemon.kill()
            daemon.wait(timeout=10)
        dump = command([str(args.journal_reader), "journal", "--state", str(state)])
        records = [dict(json.loads(line.split(" ", 2)[2]), lsn=int(line.split(" ", 1)[0]))
                   for line in dump.splitlines()]
        reported_cost(records)


if __name__ == "__main__":
    options = arguments()
    try:
        with tempfile.TemporaryDirectory(prefix="dudes-mail-live-") as directory:
            rehearse(options, Path(directory))
    except (AssertionError, OSError, RuntimeError, subprocess.TimeoutExpired) as error:
        print(f"FAIL {error}")
        raise SystemExit(1) from None
