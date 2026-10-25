# Codex fixtures

Captured `codex exec --json` output of **codex-cli 0.158.0-alpha.2.1**, recorded 2026-09-27 in an
empty scratch directory named `work`, with the default model and no user configuration. One file
per run, one JSON line per event, exactly as the process wrote it to stdout.

| fixture | command | what the run shows | turns | exit |
|---|---|---|---|---|
| `c1-success.stdout` | `codex exec --json --ignore-user-config --skip-git-repo-check --sandbox read-only -C work 'reply with the word ok'` | a success with non-zero `turn.completed.usage` | 1 | 0 |
| `c2-command.stdout` | ``codex exec --json --ignore-user-config --skip-git-repo-check --sandbox read-only -C work 'run the shell command `echo ok` and reply with its output'`` | a success with a `command_execution` item between two `agent_message` items | 1 | 0 |
| `c3-resume.stdout` | `codex exec resume 01a0e1d9-3860-7800-a9ff-2994ef3b1c6f --json --ignore-user-config --skip-git-repo-check 'reply with the word ok again'`, run from `work` | `c1`'s thread resumed: the same `thread.started` id | 1 (the thread's second) | 0 |

**`turn.completed.usage` is the thread's running total, not the turn's.** `c3` reports 32323 input,
28288 cached and 10 output tokens. The vendor's own record of that thread lists the resumed turn's
own share as 16170, 16000 and 5, and its thread total as 32323, 28288 and 10.

## Normalisation of c1 to c3

None was needed. No line carries an operator path, so every byte is what the vendor emitted, ids
and usage numbers included.

## Captures from codex-cli 0.164.0-alpha.11.1

Recorded 2026-10-25 in empty scratch folders holding two small files, with a ChatGPT login and the
flags Dudes launches with. The prompt asked for a three-step plan, one shell command, an edit of one
file, a new file and a one-word reply.

| fixture | captured from | what it shows |
|---|---|---|
| `c4-file-change.stdout` | `codex exec --json --ignore-user-config --ignore-rules -c features.plugins=false -c features.apps=false -c features.shell_snapshot=false --skip-git-repo-check -c 'sandbox_mode="workspace-write"' -- <prompt>` | a `file_change` item that starts and completes, with two paths and plain-word kinds, and a command that exits 128 with `status` `failed` |
| `app-server-turn.jsonl` | `codex app-server --listen stdio://` with Dudes's `-c` options, then `initialize`, `initialized`, `thread/start` and `turn/start` | every message the server wrote for one turn, one per line, requests left out |
| `app-server-model-list.jsonl` | the same server with no thread, after `initialize` and `initialized`, then `model/list` with empty params | the server's `initialize` answer and the `model/list` answer |

Neither run sent a plan. `update_plan` is not offered to the model under these flags, so a plan item
(`todo_list`) and a `turn/plan/updated` notification are still only hand-written in the tests.

### Normalisation of the 0.164 captures

Everything is as the process wrote it except these changes, made by a script over the whole file:

- The scratch folder is `/work`, and `codexHome` and the session file folder are the placeholder
  `/data/providers/codex`.
- Thread, turn and command ids are numbered `00000000-0000-7000-8000-0000000000NN` in order of first
  appearance, and `msg_` and `rs_` ids are numbered. Host name, installation id, plan type and
  terminal name in the user agent are neutral values, and the one MCP server's name is `docs-server`.
- In `app-server-turn.jsonl` the `thread/start` answer lists `writableRoots` of `/data/agents/a/tmp`
  and `/data/agents/a/outbox`. The capture omitted the `writable_roots` option the daemon adds, so
  Codex answered with an empty list.
- Two long `python3 -c` commands in `app-server-turn.jsonl` are shortened, and the output of the
  second is made to match.
- `app-server-model-list.jsonl` drops one model entry and one access-program name that belong to the
  capturing account. Every other model is as listed.
