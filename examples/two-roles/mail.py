#!/usr/bin/env python3
"""An unpaid two-agent mail journey through the shipped CLI and journal reader."""

import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
import traceback

MAIL_WAKE_ISSUED = "identified_mail_wake_only_turn_issued"


def command(argv, env=None):
    result = subprocess.run(argv, env=env, text=True, capture_output=True, timeout=10)
    if result.returncode:
        raise subprocess.CalledProcessError(result.returncode, argv, stderr=result.stderr)
    return result.stdout


def failure_snapshot(records, work):
    kinds = {"turn_issued", "turn_spawned", "turn_ended", "predicate_result",
             "loop_decided", "mail", "mail_state", MAIL_WAKE_ISSUED}
    counts = {kind: len(rows(records, kind)) for kind in sorted(kinds)}
    events = []
    for record in records:
        if record["t"] not in kinds:
            continue
        event = {key: record[key] for key in ("t", "agent", "turn", "pass", "provenance")
                 if key in record}
        if record["t"] == "turn_ended":
            outcome = record["outcome"]
            event["outcome"] = outcome["kind"]
            exited = record.get("exit")
            if isinstance(exited, dict):
                event["exit"] = {key: exited[key] for key in ("code", "signal") if key in exited}
        if record["t"] == "loop_decided":
            event["decision"] = record["decision"]["kind"]
        events.append(event)
    print("FAIL journal snapshot " + json.dumps({"counts": counts, "events": events[-12:]}), file=sys.stderr)
    errors = work / "agent-errors.jsonl"
    if errors.exists():
        with errors.open() as source:
            lines = source.read(4096).splitlines()
        for line in lines[:4]:
            error = json.loads(line)
            safe = {key: error[key] for key in ("exception", "line", "returncode", "errno", "io_reason") if key in error}
            print("FAIL stand-in exception " + json.dumps(safe), file=sys.stderr)
    for path in sorted((work / "agents").glob("*/ready.*.json"))[:4]:
        with path.open() as source:
            ready = json.loads(source.read(256))
        safe = {key: ready[key] for key in ("outbox_at_start", "outbox_after_wait", "inbox_at_read", "inbox_after_wait")
                if isinstance(ready.get(key), bool)}
        print("FAIL mailbox readiness " + json.dumps(safe), file=sys.stderr)


def wait_for(description, read, predicate, work):
    end = time.monotonic() + 15
    while True:
        value = read()
        if predicate(value):
            return value
        if time.monotonic() >= end:
            failure_snapshot(value, work)
            raise AssertionError(description)
        time.sleep(0.05)


def rows(records, kind):
    return [record for record in records if record["t"] == kind]


def answered(records, request):
    return request.get("act") == "request" and request.get("thread") is not None and any(
        reply.get("thread") == request["thread"] and reply["lsn"] > request["lsn"]
        and reply["from"] == request["to"] for reply in rows(records, "mail"))


def done(records, agent):
    return next((record for record in rows(records, "loop_decided")
                 if record["agent"] == agent and record["decision"]["kind"] == "done"), None)


def run(cli, runtime, work, home, config):
    work = Path(work)
    env = dict(os.environ, DUDES_RUNTIME_DIR=runtime, HOME=home,
               XDG_CONFIG_HOME=config, APPDATA=config)

    def operator(*args):
        return command([cli, *args], env)

    def journal():
        frames = map(json.loads, operator("--json", "tail", "--from", "0").splitlines())
        return [dict(frame["entry"], lsn=frame["lsn"]) for frame in frames]

    agents = {}
    for name, role in [("kim", "sender"), ("leo", "recipient")]:
        hired = json.loads(operator("--json", "hire", "--name", name, "--role", role,
                                    "--department", "infra", "--provider", "claude",
                                    "--cwd", str(work), "--usd-per-day", "5"))
        agents[role] = hired["agent"]
    sender, recipient = agents["sender"], agents["recipient"]
    for role, agent in agents.items():
        directory = work / "agents" / agent
        directory.mkdir(parents=True)
        (directory / "plan.json").write_text(json.dumps({"role": role, "peer":
                                                        recipient if role == "sender" else sender}))

    def goal(agent):
        reply = json.loads(operator("--json", "goal", "--agent", agent,
                                    "--text", "complete the mail handoff",
                                    "--usd", "1", "--hop-cap", "4", "--timeout-s", "30",
                                    "--", "test", "-f", str(work / "agents" / agent / "done")))
        return reply["goal"]

    agents["sender_goal"] = goal(sender)
    before_goal = wait_for("the sender finished and its request became durable", journal,
                           lambda records: done(records, sender) is not None
                           and len(rows(records, "mail")) == 1, work)
    assert not [record for record in before_goal
                if record["t"] in ("turn_issued", "turn_spawned", MAIL_WAKE_ISSUED)
                and record["agent"] == recipient], "mail never wakes an agent with no goal"
    assert not (work / "agents" / recipient / "started.1").exists()
    ready = json.loads((work / "agents" / sender / "ready.1.json").read_text())
    if ready["outbox_at_start"]:
        print("PASS the sender's outbox was ready before its first send")
    else:
        print("PASS the sender waited for the daemon's outbox before its first send")
    agents["recipient_goal"] = goal(recipient)
    wait_for("the reply was acknowledged after one mail wake", journal,
             lambda records: done(records, recipient) is not None
             and len(rows(records, "mail")) == 2
             and len([record for record in rows(records, "mail_state")
                      if record["state"]["state"] == "acked"]) == 2
             and len(rows(records, "turn_ended")) == 3, work)
    readiness = [json.loads(path.read_text()) for path in (work / "agents").glob("*/ready.*.json")]
    if any(ready.get("inbox_at_read") is False for ready in readiness):
        print("PASS mail reads waited for the daemon's delivered inbox markers")
    (work / "actors.json").write_text(json.dumps(agents))
    print("PASS mail to an agent with no goal created no turn before its goal was assigned")


def verify(journal_path, store_path, work):
    work = Path(work)
    agents = json.loads((work / "actors.json").read_text())
    records = []
    for line in Path(journal_path).read_text().splitlines():
        lsn, kind, payload = line.split(" ", 2)
        record = json.loads(payload)
        assert record["t"] == kind
        records.append(dict(record, lsn=int(lsn)))
    mail = rows(records, "mail")
    assert len(mail) == 2, "exactly two delivered mail records"
    request, reply = mail
    members = sorted([agents["sender"], agents["recipient"]])
    assert request["thread"] is not None and request["thread"] == reply["thread"]
    assert [record["thread_seq"] for record in mail] == [1, 2]
    assert all(record["members"] == members for record in mail)
    assert request["from"]["agent"] == agents["sender"]
    assert request["to"]["agent"] == agents["recipient"]
    assert reply["from"]["agent"] == agents["recipient"]
    assert reply["to"]["agent"] == agents["sender"]
    assert request["act"] == "request" and reply["act"] == "message"
    assert request["subject"] == "round trip request"
    assert reply["subject"] == "Re: " + request["subject"]
    assert answered(records, request), "the request's recipient later wrote in the same thread"
    assert not answered(records, reply), "a message does not expect another reply"
    recipient_goal = next(record for record in rows(records, "goal_set")
                          if record["agent"] == agents["recipient"])
    assert request["lsn"] < recipient_goal["lsn"], "the request arrived before its recipient had a goal"
    assert not [record for record in records
                if record["t"] in ("turn_issued", "turn_spawned", MAIL_WAKE_ISSUED)
                and record["agent"] == agents["recipient"]
                and record["lsn"] < recipient_goal["lsn"]], "no turn preceded the recipient's goal"
    assert done(records, agents["sender"])["lsn"] < reply["lsn"], "the reply follows sender completion"
    states = rows(records, "mail_state")
    acked = [record["id"] for record in states if record["state"]["state"] == "acked"]
    assert sorted(acked) == sorted(record["id"] for record in mail)
    assert all(record["state"]["n"] <= 5 for record in states
               if record["state"]["state"] == "notified")
    for agent in members:
        read = (work / "agents" / agent / "read.txt").read_text()
        incoming = reply if agent == agents["sender"] else request
        assert f"mail {incoming['id']}\n" in read
        assert f"act {incoming['act']}\n" in read
        assert incoming["body"] in read
        hint = f"dudes mail send --reply-to {incoming['id']} --act message --body 'Your reply'"
        assert (hint in read) == (incoming["act"] == "request")
        assert (work / "agents" / agent / "ack.txt").read_text().strip() == f"mail done {incoming['id']}"
    wakes = rows(records, MAIL_WAKE_ISSUED)
    assert len(wakes) == 1 and wakes[0]["agent"] == agents["sender"]
    assert wakes[0]["goal"] == agents["sender_goal"]
    assert wakes[0]["completion_lsn"] == done(records, agents["sender"])["lsn"]
    assert len(wakes) <= 20, "mail wakes stay within the daily cap"
    issued = rows(records, "turn_issued") + wakes
    assert len(issued) == 3
    assert sorted(record["turn"] for record in rows(records, "turn_spawned")) == sorted(
        record["turn"] for record in issued)
    goal_rows = [json.loads(line.split(" ", 2)[2]) for line in Path(store_path).read_text().splitlines()
                 if line.startswith("goals ")]
    assert len(goal_rows) == 2
    for goal_row in goal_rows:
        assert goal_row["status"] == "achieved", "both goals have the saved achieved state"
        result = goal_row["last_result"]
        assert result["pass"] is True and result["provenance"] == "observed"
        record = next(record for record in records if record["lsn"] == result["lsn"])
        assert record["t"] == "predicate_result" and record["goal"] == goal_row["id"]
        assert record["pass"] is True and record["provenance"] == "observed"
        assert record["lsn"] < goal_row["first_done"]["lsn"]
    print("PASS 2 mail records share one thread with sequences 1 and 2 and both are acknowledged")
    print("PASS the request is answered by a later journaled message from its recipient")
    print("PASS 1 mail-only wake, within the daily cap of 20; notifications stay within 5 per item")
    print("PASS 3 issued turns have exactly 3 spawns and both goals have observed completion checks")


def agent(session):
    work = Path(os.environ["DUDES_EXAMPLE_WORK"])
    cli = os.environ["DUDES_EXAMPLE_CLI"]
    identity = Path(os.environ["DUDES_MAILBOX"]).name
    directory = work / "agents" / identity
    plan = json.loads((directory / "plan.json").read_text())
    run_number = 1
    while (directory / f"started.{run_number}").exists():
        run_number += 1
    (directory / f"started.{run_number}").write_text(str(os.getpid()))
    outbox = Path(os.environ["DUDES_MAILBOX"]) / "outbox"
    ready = {"outbox_at_start": outbox.is_dir()}
    readiness = directory / f"ready.{run_number}.json"
    readiness.write_text(json.dumps(ready))
    deadline = time.monotonic() + 15
    while not outbox.is_dir():
        assert time.monotonic() < deadline, "the daemon created the agent's outbox"
        time.sleep(0.01)
    ready["outbox_after_wait"] = True
    readiness.write_text(json.dumps(ready))
    queued = None
    if plan["role"] == "sender" and run_number == 1:
        queued = command([cli, "mail", "send", "--to", plan["peer"], "--act", "request",
                          "--subject", "round trip request", "--body", "Please reply when you read this."])
    else:
        inbox = Path(os.environ["DUDES_MAILBOX"]) / "inbox"
        ready["inbox_at_read"] = any(inbox.glob("*.json"))
        readiness.write_text(json.dumps(ready))
        while not any(inbox.glob("*.json")):
            assert time.monotonic() < deadline, "the daemon delivered an inbox marker"
            time.sleep(0.01)
        ready["inbox_after_wait"] = True
        readiness.write_text(json.dumps(ready))
        read = command([cli, "mail", "read"])
        (directory / "read.txt").write_text(read)
        delivered = re.search(r"^mail ([0-9A-Z]{26})$", read, re.MULTILINE)
        assert delivered is not None, "the agent read its delivered mail id"
        incoming = delivered.group(1)
        if plan["role"] == "recipient":
            queued = command([cli, "mail", "send", "--reply-to", incoming, "--act", "message",
                              "--body", "Reply received."])
        ack = command([cli, "mail", "done", incoming])
        (directory / "ack.txt").write_text(ack)
    if queued is not None:
        receipt = re.fullmatch(r"mail queued ([0-9A-Z]{26})\n", queued)
        assert receipt is not None, "the CLI reported the queued outbox id"
        pending = outbox / f"{receipt.group(1)}.json"
        deadline = time.monotonic() + 3
        while pending.exists():
            assert time.monotonic() < deadline, "the daemon consumed the queued mail before the sender exited"
            time.sleep(0.01)
    (directory / "done").touch()
    transcripts = work / "transcripts"
    transcripts.mkdir(exist_ok=True)
    transcript = transcripts / f"{session}.jsonl"
    with transcript.open("a") as out:
        out.write(json.dumps({"session": session, "agent": identity, "pid": os.getpid()}) + "\n")
    uses = len(transcript.read_text().splitlines())
    print(json.dumps({"type": "result", "subtype": "success", "is_error": False,
                      "terminal_reason": "completed", "session_id": session,
                      "result": f"mail {plan['role']} turn {run_number}", "total_cost_usd": uses / 100,
                      "modelUsage": {"stand-in": {"inputTokens": uses * 10,
                                                   "outputTokens": uses * 20}}}))


if __name__ == "__main__":
    try:
        {"run": run, "verify": verify, "agent": agent}[sys.argv[1]](*sys.argv[2:])
    except Exception as error:
        where = traceback.extract_tb(error.__traceback__)[-1]
        if sys.argv[1] == "agent":
            details = {"exception": type(error).__name__, "line": where.lineno}
            if isinstance(error, subprocess.CalledProcessError):
                details["returncode"] = error.returncode
                text = error.stderr or ""
                errno = re.search(r"\(os error ([0-9]+)\)", text)
                if errno:
                    details["errno"] = int(errno.group(1))
                if "entity not found" in text or "No such file or directory" in text:
                    details["io_reason"] = "not_found"
            with (Path(os.environ["DUDES_EXAMPLE_WORK"]) / "agent-errors.jsonl").open("a") as out:
                out.write(json.dumps(details) + "\n")
        message = (f"stand-in command exited {error.returncode}" if isinstance(error, subprocess.CalledProcessError)
                   else str(error) or where.line)
        print(f"FAIL {message}", file=sys.stderr)
        sys.exit(1)
