# Two-agent examples

These scripts run two managed agents toward a goal with an unpaid stand-in provider. The crash
arms kill a real daemon around native launch, restart it, and require explicit operator consent
before repeating an interrupted issue. A missing Spawn never proves that no child or external
effect ran. Recovery preserves one logical issue; it does not guarantee exactly-once execution.

It needs **no account with any model vendor** and talks to no network. The agent it hires is a
stand-in — `bin/claude`, a short shell script that speaks the wire format a real coding-agent CLI
speaks. Everything else in the transcript is the software as it ships: the daemon binary, the
command-line client, the journal, the projections. Each arm gives its daemon an operator file of
its own that sets only the per-turn reserve a hire with a dollar ceiling needs, so an arm never
reads your configuration and runs the same on a machine that has none.

## Run it

Use a POSIX shell, the repository's Rust toolchain, and Python 3.8 or later. The crash examples use
only Python's standard library to compare JSON identities and exact integer journal positions.
From a clean checkout:

```sh
cargo build --release --workspace

./examples/two-roles/run.sh               # arm 1
./examples/two-roles/crash.sh             # arm 2
./examples/two-roles/crash.sh --no-crash  # the control — run it after arm 2
./examples/two-roles/crash-after-spawn.sh # the post-Spawn arm
```

Or the first three at once, which is also how they stay runnable rather than becoming a thing
somebody once ran:

```sh
cargo xtask gate
```

`cargo xtask gate` runs arm 1, arm 2 and the control. It does **not** run the post-`Spawn` arm:
the post-`Spawn` arm is not yet part of `cargo xtask gate`. Run it directly — it takes
the same environment overrides, prints the same `PASS`/`FAIL` lines, exits non-zero the same way,
and leaves its evidence in the same place.

Each script prints one line per thing it checked, `PASS` or `FAIL`, and exits non-zero if anything
failed. Journals and projection exports remain under `target/two-roles-evidence/`. Recovery
inspections, request IDs, submission logs and receipts use an attempt-specific filename prefix, so
a later run does not overwrite an earlier failed submission's logs.

The arms are ordered. `crash.sh --no-crash` compares its own run against the artefacts the crashed
run left, and says so plainly if you have not run `crash.sh` first.

## Rehearse agent mail

After the same release build, run the mail example independently:

```sh
./examples/two-roles/mail.sh
```

Kim receives a goal and sends Leo a request. Leo has no goal yet, so the request reaches his
mailbox without starting a turn. After Kim's goal reaches Done, the script checks that Leo has
neither an issued nor a spawned turn, then gives him a goal.

Leo uses `dudes mail read`, replies with the delivered mail id, and acknowledges the request with
`dudes mail done`. The request's read output includes its act and a reply command. The reply omits
`--subject`, so it inherits `Re: round trip request`. Reading alone does not acknowledge mail.
The reply wakes Kim once after his
goal has finished; he reads and acknowledges it. An agent hired without a goal is never woken by
mail, so give it a goal before expecting that automatic response.

Mailbox files update asynchronously. The stand-in waits for the daemon-created outbox before
sending and for a delivered inbox marker before its single read. It does not assume that a
journaled receipt is already readable through the mailbox files.

The stopped daemon's journal must contain exactly two mail records on one thread, numbered 1
and 2, with both marked acknowledged. The first has act `request`; the later message from its
recipient makes that request answered. This answer is derived from the journal, separately from
acknowledgement. The offline CLI's current inbox files do not establish answered status.
The journal must also contain one mail-only wake and three issued
turns with three matching spawns. Both goals' saved results must point to observed passing
predicate records before their Done decisions. The daily wake and per-item notification counts
must stay within their caps.

The script uses a scratch home and state, removes them afterward, and needs no vendor account.
`DUDED`, `DUDES_CLI` and `DUDES_XTASK` can select binaries from another build directory.

### Rehearse with a live engine

`mail-live.py` uses the installed Claude or Codex engine on your PATH. It starts two real agents
and can bill your engine account. Run it only after authorizing that spend.

Prepare an absolute scratch home and sign the engine in there through its own sign-in flow.
For Claude, use `HOME` at that home and `CLAUDE_CONFIG_DIR` at its `.claude` child. For Codex,
use `HOME` at that home and `CODEX_HOME` at its `.codex` child. Do not copy credential files.
Then run, substituting that prepared directory:

```sh
python3 examples/two-roles/mail-live.py --allow-spend --provider claude \
    --engine-home /absolute/path/to/prepared-scratch-home
```

Use `--provider codex` for Codex. `--cli`, `--daemon` and `--journal-reader` select absolute build
outputs when they are outside `target/release`. The journal reader runs from this checkout and
must be built from it. The script gives Dudes its own temporary home,
configuration, state, runtime and project, and passes only the selected scratch engine profiles
to the adapters. It removes the Dudes directories afterward and keeps the prepared engine home.

The recipient finishes a small preparation goal before the sender mails it. The script then
requires a request, a mail wake, and a later reply from that recipient in the same journaled
thread. It derives answered from those records. It does not require a stored answer state or a
browser display. This run does not verify an answered label in CLI or Floor output. A request
that receives no reply fails the rehearsal.

The floor admission ceiling is $1 shared by both agents, with $1 per agent per day and $1 per goal.
Each goal allows one ordinary turn, and each completion check times out after 60 seconds.
The script waits up to 90 seconds for each expected outcome. The dollar amounts are admission ceilings.
A running turn can finish above them. The script prints engine-reported dollars after shutdown and identifies
unreported usage as unknown. Its live-engine path is opt-in; the unpaid `mail.sh` remains the
repeatable stand-in check.

## What each arm proves

**Arm 1 — `run.sh`. A goal is finished because a program said so, not because an agent said so.**
Two agents are hired, each with a role. One is given a goal and a command to run that decides
whether the goal is met. Nobody types anything at anything after that. The agent's first turn
deliberately leaves the work undone, so the deciding command is run and comes out **false**, and
the loop takes another turn; the second turn does the work, the command comes out **true**, and the
goal closes. What makes this worth running is the negative half: the goal is closed by a command
the daemon ran and watched, and *never* by the agent reporting success. An agent that said "done"
would be believed by no part of this. (The rule is invariant 3, "Done is Observed".)

**Arm 2, `crash.sh`, requires consent before repeating an issue with no recorded Spawn.**
The first child completes without meeting the goal. The daemon then stops after the next
`TurnIssued` is durable and before its native launch. The script proves that window has no child,
sends a real `SIGKILL`, and retains the journal and projection left behind.

After restart, the script requires a durable `RecoveryRequired` for that same issue. Two journal
reads around a real `dudes --json recovery inspect` call must show no authorization or replacement
Spawn, and the next stand-in must not have started. The saved inspection is submitted exactly once
with `--choice repeat-issued-fresh`. The CLI's preprinted request ID and complete submission logs
are retained. An ID-only receipt lookup must confirm that original choice before the script checks
the expected held replacement child. Submission may already have spawned that child before the
receipt RPC returns. The stand-in's release gate holds its goal effect until the script releases it.
An uncertain reply is followed only by receipt lookup, never an automatic resubmission.

The replacement must retain the original TurnId, task and ordered answers, add no second logical
issue, record one `reissued` Spawn, and use the authorized fresh context. Its observed predicate
must close the goal. Both final projection exports must match replay from their own journals.

**The control, `crash.sh --no-crash`, completes without a recovery request.**
It runs the same feature-built daemon without an armed fault. The ordinary turn sequence must
complete the goal with no recovery event or authority cell. It compares its journal and projection
with a recent, single-use crashed run's retained artifacts.

**The post-launch arm, `crash-after-spawn.sh`, shows the old child ending with the daemon.**
The fault parks after the first native child exists and before its Spawn record. The stand-in's
own marker and the process table establish that a child exists; the offline journal must contain
its issue and no Spawn or session confirmation.

After the SIGKILL the script waits until the OS no longer reports that child's PID. On macOS and
Linux the keeper the daemon started beside the child ends the child's process group within
milliseconds of the daemon's death, so no record is needed to find it. This is an OS-observed
absence, not this shell reaping a process it did not parent. The daemon then restarts, and the
script checks the Required barrier and the absence of an automatic replacement. Only then does the
script submit one explicit fresh repeat and verify the retained request's receipt. The two
sequential physical attempts must use distinct session files, each containing only its own
process's events. The journal must retain the original issue and identify the replacement Spawn.
There is no overlap between the old child and its replacement.

These stand-ins verify session identifiers and file separation. They do not prove that a real
provider restored conversation history, or that the old child had no earlier external effects.

You will see the shell report `Killed: 9` in the middle of arm 2 and of the post-`Spawn` arm. That
is the kill being reported by your shell, and it is the point of those arms.

## How the crash is made to land

The window arm 2 needs is real but a few microseconds wide: the daemon writes the turn to disk and
launches it almost in the same breath. Killing from the outside and hoping to land inside that
window is not a test, it is a coin toss — and a check that only sometimes lands is a bug, not
something to retry.

The crash arms build the daemon with `--features fault-injection` and use its launch wrappers to
**stop at one chosen launch, and leave a file saying it has stopped.** A wrapper kills nothing and
changes nothing else. The script waits for that file and
then sends a real `SIGKILL` from outside, exactly as a person would. What the daemon has on disk at
that moment is precisely the window's state.

There are two launch stall points, because a launch has two windows and the two arms are about different
ones:

| | armed with | what exists when the signal lands |
|---|---|---|
| `crash.sh` | `DUDES_FAULT_STALL_BEFORE_SPAWN` | the turn committed, **no process** |
| `crash-after-spawn.sh` | `DUDES_FAULT_STALL_AFTER_SPAWN` | the turn committed, **a live child no record names** |

Two things are worth being explicit about.

- The feature is **off by default and is not turned on by `cargo build --release --workspace`**, so
  the daemon that build produces has no fault in it. `crash.sh` builds its own copy into
  `target/two-roles-evidence/fault/` rather than over `target/release/duded`, so the binary arm 1 runs is never
  replaced by one that can stall.

  `bins/duded/tests/dev_features.rs` checks workspace manifests for accidental defaults and
  dependency feature enablement. Deliberately passing `--features fault-injection` still creates
  an instrumented binary and is disclosed by each crash script.
- Both the crashed run and the control run **the same feature-built binary**. The only difference
  between them is whether the fault is armed, which is what makes the comparison mean something.

One consequence is visible in the post-`Spawn` arm's output and is worth knowing before it puzzles
you: it sets its goal from a call it never gets an answer to. That arm stops the daemon at the
agent's *first* launch, and the loop that runs a launch is the same one, on the same thread, that
answers the operator call which caused it. So the reply cannot come, by construction. The record was
made durable before the launch was attempted, so the goal exists regardless — and the arm asserts
that from the journal rather than assuming it.

## How to see the arms fail

A check that has never been seen to fail is not yet evidence. Each of these should go red:

```sh
MARK_ON_RUN=99 ./examples/two-roles/run.sh    # the agent never does the work: no goal is ever met
MARK_ON_RUN=99 ./examples/two-roles/crash.sh  # same, through the crash
MARK_ON_RUN=99 ./examples/two-roles/crash-after-spawn.sh
STALL_NTH=9 ./examples/two-roles/crash-after-spawn.sh  # a launch the run never reaches: nothing
                                                       # is ever in the window, so nothing is killed
```

Inside the arms, several checks are paired with their own controls and you will see them in the
output: the deciding command is asserted to have come out false at least once (otherwise "it came
out true" would say nothing); the projection comparison is repeated against a projection that is
deliberately behind, which must come out different; the comparison between the two runs is repeated
against the same run's own two snapshots, which must come out different too; and in the post-`Spawn`
arm each child's process id is asserted to be **found** in its own file before anything is said
about where it is absent, and the three names that arm's two absence checks search the journal for —
two record kinds and one field — are each written once and checked, under that same name, against
the generated record contract, so that a name misspelled in a search is misspelled in its control
too and the arm goes red instead of reporting the absence it went looking for.

## Why the two runs' state is compared by shape and not for equality

The obvious way to compare a crashed run with an uncrashed one is to demand their final state come
out identical. That would be the wrong demand, and a system that satisfied it would be worse:

- the crashed run's turn is marked as having been taken up by recovery, and the uncrashed run's is
  not. A state in which those two matched would be one that had **lost** the fact that a turn was
  re-issued — the single thing arm 2 exists to observe; and
- every identifier in a run is minted fresh, so two runs share no agent, no goal, no turn and no
  timestamp anyway.

So the control compares the two runs' state by **shape**: the same tables, the same number of ordinary rows
in each, and the same goal status. Every part of that is a positive statement about something
named, which is what makes it safe as the system grows — a field added to a row cannot turn it red
for a reason that is not a defect, and anything it does not mention is visibly unasserted rather
than quietly uncompared. Beside it the script prints the measurement the paragraph above rests on,
including which turn rows carry the re-issue mark in each run, so a reader can check the reasoning
instead of taking it.

Three tables receive dedicated checks instead of an equal-row-count assertion.

A fresh repeat confirms an additional session, so `session_totals` has one extra row in the crash
run. The control identifies that row by the replacement Spawn's session key.

The `instructions` table retains the earlier attempt's baseline and uncertainty beside its completed
retry. The existing typed checker compares each run's complete cells with its journal boundaries.

The `recovery` table also contains ordinary source witnesses in the control. It is never required
to be empty. The crash checker requires exactly one Required and one Authorized event, joins the
original issue and retained request through their consumed Spawn, and validates the exact episode,
episode claim, current work, authorization, claim, request and cut cells by key and value. A repeat
has no planned-turn cell. Every exported witness must name its actual journal source, and the
original issue witness must equal the authorization's complete immutable issue witness. The
ordinary control must have source witnesses and no recovery event or authority cell. Each run
still compares every exported row with a full replay of its own journal.

## Windows

The Windows arm reads the process table outside the daemon with PowerShell/CIM. It snapshots only a
**live** daemon's direct children, requires that daemon's parent row, and accepts a candidate only
when its `CreationDate` is no earlier than the parent's. That last check prevents a reused
`ParentProcessId` from becoming a false child. The collector emits one sorted PID set; the exact-one
assertion uses that same set for both its count and the stand-in PID it expects.

The collector is deliberately not an orphan census. Arm 2 makes it exact by proving the pre-kill
stall window has zero children, so the kill cannot leave an old child to be reparented. After explicit
consent, the script counts the restarted, live daemon. The post-launch arm retains the unrecorded
old child through its first restart, then observes its absence before consent.

On a native Windows runner, run the normal transcript and its control:

```sh
./examples/two-roles/crash.sh
./examples/two-roles/crash.sh --no-crash
```

The retained counting controls must each fail with exactly the exact-one assertion; they are evidence
that the collector distinguishes the failing cardinalities rather than merely returning a number:

```sh
./examples/two-roles/crash.sh --child-count-control=zero # exits 1 after recording count 0
./examples/two-roles/crash.sh --child-count-control=two  # exits 1 after recording two PIDs
```

The collector's own failure branches are a third control, and it runs anywhere — it needs no daemon,
no release build and no Windows. It calls the collector with a stand-in for the PowerShell step and
asserts five things: a step that produced records collects them; a step that produced none is an
empty collection rather than a refusal; a receipt naming another pid is refused; child records that
cannot be read *after* a successful step are a refusal; and that refusal reaches the snapshot as a
failure with no count, never as a count of zero. The last two are the distinction the whole arm
rests on — an unreadable file presented as an empty set is indistinguishable from a daemon with no
children, and the pre-kill window asserts exactly zero children.

```sh
./examples/two-roles/crash.sh --child-count-control=read-failure # exits 0; five propositions
```

## The files

| file | what it is |
|---|---|
| `run.sh` | arm 1 |
| `crash.sh` | arm 2, and the control with `--no-crash` |
| `crash-after-spawn.sh` | the post-`Spawn` arm |
| `lib.sh` | the harness they share: starting a real daemon, driving the real client, counting real processes, reading the transcripts back |
| `bin/claude` | the stand-in agent — announces a session, writes that session's transcript, does the work on the run it is asked to, reports a result |

`lib.sh` decides nothing the daemon decides. A harness that carried its own copy of the loop's rules
would keep passing after the daemon drifted away from it, which is the one failure this kind of
script must not have.
