# Two roles, one goal, and a daemon killed mid-turn

This directory is a **transcript you run**, not a test suite that goes green somewhere you cannot
see. It is the first demonstration of the project: two agents work a goal to
completion with nobody watching, and the daemon survives being killed at the worst possible moment
without losing the work or doing it twice.

It needs **no account with any model vendor** and talks to no network. The agent it hires is a
stand-in — `bin/claude`, a short shell script that speaks the wire format a real coding-agent CLI
speaks. Everything else in the transcript is the software as it ships: the daemon binary, the
command-line client, the journal, the projections. Each arm gives its daemon an operator file of
its own that sets only the per-turn reserve a hire with a dollar ceiling needs, so an arm never
reads your configuration and runs the same on a machine that has none.

## Run it

From a clean checkout, on a machine that has never built this repository:

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
failed. Nothing is left behind but a directory of evidence under `target/two-roles-evidence/`: the journals
and projection exports each arm produced, kept so a failure can be read after the fact by someone
who was not at the keyboard.

The arms are ordered. `crash.sh --no-crash` compares its own run against the artefacts the crashed
run left, and says so plainly if you have not run `crash.sh` first.

## What each arm proves

**Arm 1 — `run.sh`. A goal is finished because a program said so, not because an agent said so.**
Two agents are hired, each with a role. One is given a goal and a command to run that decides
whether the goal is met. Nobody types anything at anything after that. The agent's first turn
deliberately leaves the work undone, so the deciding command is run and comes out **false**, and
the loop takes another turn; the second turn does the work, the command comes out **true**, and the
goal closes. What makes this worth running is the negative half: the goal is closed by a command
the daemon ran and watched, and *never* by the agent reporting success. An agent that said "done"
would be believed by no part of this. (The rule is invariant 3, "Done is Observed".)

**Arm 2 — `crash.sh`. A turn is written down before it is started, so a crash can lose neither the
work nor the fact that it was already begun.** The daemon is killed — a real `SIGKILL`, no cleanup,
no chance to save anything — at the one instant where losing state would be worst: after it has
committed a turn to disk and before it has launched the process for it. It is then restarted on
exactly what it left behind. The turn is taken up exactly once: not dropped, and not run a second
time beside the first. The arm asserts a child existed **before** the kill, so "exactly one child
afterwards" cannot be satisfied by a harness that never launches anything; it asserts the state the
crash left really was a turn written and unlaunched; and it asserts that everything the daemon
rebuilt from its log matches what it had. (Invariant 2, "Persist before Spawn", and invariant 1,
"the log is the only writer".)

**The control — `crash.sh --no-crash`. The kill actually landed.** The same script with nothing
killed. Without it, arm 2 proves very little: a harness that only ever produced one child would
pass every count in it. The control shows that the two runs are genuinely different in the way that
matters — the crashed run has a turn that was written and never launched, and the run that was not
crashed never has one — and that both nonetheless finish the goal and spawn exactly one process per
turn they issued.

**The post-`Spawn` arm — `crash-after-spawn.sh`. A crash after the child exists leaves two
children in two separate conversations, not one conversation with both of them writing into it.**
A launch has two windows a crash can land in, and arm 2 takes the first: the turn is on disk and
nothing has been started. This arm takes the second. The child is running — and the record that
would name its process and the session it was launched on is exactly the record the crash prevents,
so nothing in the log names it at all. The daemon restarts, finds a turn it has to take up again,
and cannot know that the first child is alive and still writing.

What it must not do is send the second child into the first one's session. Two processes appending
to one conversation produce a transcript that reads as neither of theirs, and a next turn that
resumes it inherits the mess. So the re-issued turn announces a **freshly minted** session, and this
arm goes and looks at the files: exactly two, one per child, and neither holding the other's events.

It also checks the things without which that would say nothing — that the first child really did
outlive the kill (nothing reaps it: a killed daemon runs no exit path); that both children were
alive at the same moment; that the two files on disk are the two sessions the log names for that
turn, so a count of two is about the right two; and that each child's process id **is** found in
its own file. That last one is the positive half every absence needs: a check looking in the wrong
directory, or for the wrong field, finds nothing anywhere, and would report "no file holds both"
about a directory it cannot read.

Measured, because a check nobody has seen fail is not yet evidence: with the re-issue changed back
to resuming the session the turn was issued with — the earlier behaviour — four of this
arm's checks go red, and the one that names the harm reads `FAIL both children wrote into:
<one session>`.

**What this arm does not claim.** It runs in the branch where the turn being re-issued has no
vendor-confirmed session behind it, and it asserts nothing about the branch where one has been
confirmed. That is a property of the fault window: the host parks the decide loop inside `spawn`
after the child starts and before `TurnSpawned` can be persisted. The stream watcher may enqueue
the child's opening line while the loop is parked, but that loop cannot process the queued
`SessionLearned` before the external kill. Ordinary managed turns do process the opening line and
resume the confirmed session on their next turn. The check that says no confirmation reached this
journal records which branch the crash left; it does not choose between two branches itself.

You will see the shell report `Killed: 9` in the middle of arm 2 and of the post-`Spawn` arm. That
is the kill being reported by your shell, and it is the point of those arms.

## How the crash is made to land

The window arm 2 needs is real but a few microseconds wide: the daemon writes the turn to disk and
launches it almost in the same breath. Killing from the outside and hoping to land inside that
window is not a test, it is a coin toss — and a check that only sometimes lands is a bug, not
something to retry.

So the daemon used by the crash arms is built with one extra feature (`--features
fault-injection`) that adds a single ability: **stop at one chosen launch, and leave a file saying
you have stopped.** It kills nothing and changes nothing else. The script waits for that file and
then sends a real `SIGKILL` from outside, exactly as a person would. What the daemon has on disk at
that moment is precisely the window's state.

There are two stall points, because a launch has two windows and the two arms are about different
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

  Stated as what is true and checked, because the stronger-sounding version — "the released daemon
  cannot contain this code" — is a build-graph claim, and a build graph changes. Cargo turns a
  feature on for the whole build if *any* package in it asks, so one dependency line elsewhere in
  the workspace would be enough to put the fault into an ordinary release, and nothing today would
  fail if it did. Making that a checked fact is a separate piece of work; until
  it exists, the guarantee here is the build command, not the compiler.
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

Two tables' row counts differ on purpose. A re-issued turn launches on a freshly minted session
rather than resuming the one its agent had, and `session_totals` keeps one row per confirmed
session. So the crashed run holds exactly one `session_totals` row more than the control, and the
control asserts that row by its key: it is the session the re-issued spawn announced. A re-issue
that resumed the old session would leave the counts equal, and the control would fail.

The `instructions` table also retains the earlier attempt's baseline, explicit recovery Unknown
and unresolved boundary alongside the completed retry. A typed checker derives the complete cell
map from each run's recorded boundaries, then checks the earlier unspawned attempt and its exact
same-turn retry independently. It accounts for those four extra cells without discarding the old
uncertainty or accepting unrelated rows, missing cells, duplicate keys or incorrect issue and
native receipt links. The store export must still equal replay from its own journal in each run.

## Windows

The Windows arm reads the process table outside the daemon with PowerShell/CIM. It snapshots only a
**live** daemon's direct children, requires that daemon's parent row, and accepts a candidate only
when its `CreationDate` is no earlier than the parent's. That last check prevents a reused
`ParentProcessId` from becoming a false child. The collector emits one sorted PID set; the exact-one
assertion uses that same set for both its count and the stand-in PID it expects.

The collector is deliberately not an orphan census. Arm 2 makes it exact by proving the pre-kill
stall window has zero children, so the kill cannot leave an old child to be reparented. After replay,
the script counts the restarted, live daemon. The post-Spawn arm has a real orphan by design and
uses its own transcript and PID assertions for that fact.

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
