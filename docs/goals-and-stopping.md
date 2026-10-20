# Goals, completion checks and stopping

## Completion checks

A goal is marked Done only when the daemon's own run of its completion check passes. What an agent reports
about its own work never marks a goal Done.

Before the agent's first turn, the daemon runs the check once and refuses the goal if the check
already passes, because that check could not show when the goal is done. A check that should keep
passing, such as a test suite, can be declared with `--check-may-already-pass`. It is then not run
first, and Done means only that it passes after the agent's work.

Each project offers the Floor its own check and any more you add with
`dudes project check add`. A Floor hire or goal change names the one its goal uses.

The completion check runs in the agent's working directory with no sandbox. See
[What agents can do on your machine](what-agents-can-do.md).

## Replace a goal

To replace a current goal and its completion check together, use
`dudes goal replace --request <path>`. Review the JSON file's literal text and command locally.
The request contains `request_id`, `agent`, `expected_goal`, `text`, `predicate`, and `check`,
and optionally `may_already_pass`. `predicate` is a command with `argv` and `timeout_s`. `check`
contains a public-safe `id` and `label`. The new check runs once first, as it does for
`dudes goal`, unless `may_already_pass` is `true`. The exact shape is published as
`GoalReplaceParams` in `schemas/rpc.schema.json`.
Read the current goal id with `dudes --json agents`. Choose a fresh 64-character lowercase
hexadecimal request id for each proposal, and retain that exact file for retries.

Replacement refuses unsettled work and stale goals, and preserves remaining optional budgets and
hops. It never resumes the agent automatically. The Floor shows only the configured check summary.
Its goal form keeps the existing check or names another check set up for the agent's project,
and runs it once first.

## Pause, resume, halt and retire

`dudes pause <agent id>` holds future turns while current work settles. When ready,
`dudes resume <agent id>` clears pause, halt, and automatic hold.

`dudes halt --agent <agent id>` records a halt and requests teardown of the agent's owned
children. `dudes resume <agent id>` clears that halt, together with pause and automatic hold.

`dudes retire --agent <agent id>` retires the agent. On an interactive terminal, confirmation
defaults to no. Scripts and `--json` require explicit `--yes`. Resume cannot undo retirement.
A halt or retirement acknowledgement confirms the recorded control; owned-child teardown may
still be in progress.

## Recover an interrupted goal

Recovery keeps the same Managed agent and unfinished goal. Ordinary `dudes resume` does not
release a recovery barrier. Inspect the opportunity and save its JSON before choosing context:

```sh
dudes --json recovery inspect <agent-id> > inspection.json
```

Read `inspection.json`, then submit one choice that the inspection offers:

```sh
dudes recovery submit inspection.json --choice <offered-choice>
```

Replace `<offered-choice>` with one of the offered names:

- `try-recorded` attempts the recorded conversation. It does not guarantee restored provider history.
- `fresh-with-recap` starts fresh with a recorded recap, which may shrink or be omitted to fit the task.
  Old conversation history is not restored.
- `repeat-issued-fresh` repeats the exact already-issued task in fresh context. A child may have run
  before its launch was recorded, so earlier effects may repeat. The task and answers stay unchanged,
  and no recap is added.

Submission uses the saved inspection without refreshing it. Authorization does not guarantee a
launch. Budgets, hop caps, questions, instruction review, and other admission gates still apply.

Submit prints its request ID to standard error before sending. Keep that ID. If the reply is lost
or authorization is unknown, read the receipt with the retained ID:

```sh
dudes recovery receipt <request-id>
```

Receipt lookup is read-only and never retries recovery. Do not resend the submission.
A later eligible recovery episode needs a fresh inspection and explicit choice.

### Make a new choice after unused consent is invalidated

If browser consent becomes `invalidated` before a turn is issued, keep its request ID and receipt.
Ordinary Resume and waiting do not reopen that consent. Its unused planned turn cannot be issued.

If the last turn ended with an Observed `Failed` or `NoResult` outcome, a later explicit Pause can
make recovery available for the same unfinished Managed goal. Pause does not make an unstarted
`NotLaunched` turn from a `LaunchFailed` hold eligible.

After the invalidation, pause the agent:

```sh
dudes pause <agent-id>
```

Save a fresh inspection in a new file:

```sh
dudes --json recovery inspect <agent-id> > inspection-after-invalidation.json
```

Read the new file. Only if it offers recovery, choose an offered context and submit a new request:

```sh
dudes recovery submit inspection-after-invalidation.json --choice <offered-choice>
```

If no choice is offered, do not submit a recovery request. Do not reuse the old inspection or
resend the old submission.
The [recovery admission gates](#recover-an-interrupted-goal) still apply.

## Stop the daemon and closing time

```sh
dudes stop
```

`stop` starts closing time. When an agent's running turn ends, the daemon gives that agent one
more task: save its work, note its next steps and acknowledge. The daemon waits until every
such agent has acknowledged and finished, for at most 360 seconds. An agent whose goal is done
or cleared, whose goal was replaced, or that was retired is never given the task, so the daemon
does not wait for it. A turn still running at the bound is ended without the task. The daemon
then stops, and `stop` prints how many agents acknowledged, names each agent it stopped waiting
for with the reason, and lists the ids of any other agent that did not acknowledge. An
acknowledgement means the agent answered, not that its work is saved. `dudes stop --hard` skips
closing time and ends running turns at once.

Once the daemon has run a managed turn, `stop` warns that it cannot prove that nothing an agent
process started left its process group or job. That warning alone still means success. Any
other teardown failure is an error, reported after the daemon closed its control socket.
`dudes ping` reports whether the daemon still answers.
