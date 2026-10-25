# Pi fixtures

Captured standard output of **Pi 0.85.1** in JSON mode. One file per run, one JSON line per line
the process wrote, exactly as it wrote them apart from the normalisation below. Every run exited 0.
`tests/pi_golden.rs` holds what the reader must conclude from each.

Files prefixed `v1-` were captured from **Pi 1.0.4** on 2026-10-07 using a loopback
stub provider, dummy credentials and throwaway homes and working directories. They cover a
single call, a read-tool call, two turns of a resumed session, a retry after HTTP 429, a compaction,
and a failed call after HTTP 401. No account or paid model was used. The retry includes the new
`entry_appended` event; it must neither end the turn nor charge a call. Several other runs also
retried an initial connection error before the successful stub call.

Files prefixed `p` were captured from **Pi 1.4.0** on 25 October 2026 with the flags Dudes passes
(`--mode json --no-extensions --no-skills --no-prompt-templates --no-approve --session-dir <scratch> --`
and the task). Pi 1.4.0 adds a `system` message with `sections`, and an `aborted` field on
`agent_settled`. The captures replace the working directory with `/work` and each long system prompt
section with a short placeholder, and keep every other byte. `p1-model-error.jsonl` ran with the
OpenAI Codex provider on a ChatGPT account, which rejects the model: the one assistant message has
empty `content`, `stopReason: "error"` and an `errorMessage` that holds the provider's JSON body,
and the process still exits 0. `p2-bash-tool.jsonl` ran with the `openai` provider, model
`gpt-6.1-sol`, and shows one `bash` call and a final reply of `there are 3 files`; 85 of its 100
`message_update` lines are dropped to keep the file small.

Each run ran in a throwaway working directory. The runs captured on 2026-09-27 passed `--mode json
--no-extensions --no-skills --no-prompt-templates --no-approve --no-context-files` and ended their
argv with `--` and the task as one element. `h1-retry` was captured on 2026-09-16 with `--mode json
--no-session --provider stub --model stub-1 "first task"`. A re-run on 2026-09-27 with the flags
above wrote the same 21 event types in the same order.

| fixture | run | what it shows |
|---|---|---|
| `l1-success.jsonl` | live, task `Reply with exactly OK.` | one call. Every `message_update` reports zero tokens, and the call's figure first appears on `message_end` |
| `l2-tool.jsonl` | live, task `Use the read tool to read note.txt, then reply with only the code word it contains.` | two calls: one that calls the `read` tool, then the answer. `turn_end` restates each call's figure |
| `l3-turn1.jsonl` | live, task `Remember the word PLUM` | the first turn of a persisted session |
| `l3-turn2.jsonl` | live, `--session <id of l3-turn1>`, task `Which word did I ask you to remember?` | the resumed turn: the same session id, only its own messages and usage, and the model recalls the word |
| `h1-retry.jsonl` | hermetic, the provider's first call fails with a retryable 429 | `agent_end` with `willRetry: true`, an automatic retry, a second `agent_end`, then one `agent_settled`. Each `message_update` of the successful call restates its figure |
| `c1-compaction.jsonl` | hermetic, `--session-dir <scratch>`, task `Remember the word PLUM`, with a 60-token context window and compaction settings `reserveTokens` 50 and `keepRecentTokens` 1 | Pi compacts after `agent_end` and before `agent_settled`. Its summary call reaches the stub as a second request, and no message event carries it. The call's figure arrives only on `compaction_end`, as `result.usage` |
| `e1-login-expired.jsonl` | live, task `Reply with exactly OK.`, while the provider login had expired | the one assistant message ends with `stopReason: "error"` and reports zero tokens, and the process still exits 0 |

The live runs also passed `--tools read` and a scratch `--session-dir`. The `l3` pair shared one
`--session-dir`. They used Pi's own configured provider and credentials. The hermetic runs pointed
Pi at a loopback stub provider through a throwaway `PI_CODING_AGENT_DIR` whose `models.json` sets
the provider's `baseUrl` to `127.0.0.1`, so no account was used and nothing was spent. The stub
answers `stub reply <n>` to its `n`th request and reports `11 × n` input tokens and 3 output
tokens, except in `h1-retry`, where it reports 10 and 3. `c1-compaction` set its context window
in that `models.json` and its compaction settings in the same directory's `settings.json`.

## Normalisation

The session header's `cwd`, and any other occurrence of the capture's working directory, is
replaced with `/tmp/dudes-fixture`. Every other byte, including ids, timestamps, usage numbers and
error text, is what Pi wrote. Before a run was kept, a key scan ran over it and found nothing, and
the same scan found each of ten planted key shapes in a control text. A test asserts that no line
carries a home or temporary directory path.

The 1.0.4 captures had already replaced the scratch directory with `<scratch>`; these fixtures
replace that marker with `/tmp/dudes-fixture` and the package installation directory with
`/opt/pi`, and rename the planted repository-note marker to `AGENTS-CONTEXT-FIXTURE`.
All other captured bytes are retained. These historical captures test stream parsing;
the current acceptance runner separately checks raw bytes before redaction for secret leaks.
