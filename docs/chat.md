# Chat

Chat is always on and needs no setup. Open the Floor, select an agent and type in **Chat**.
Anyone holding a Floor session can send messages this way, and each one starts an ordinary turn
that counts against any spending ceiling you have set. Older configuration files may still carry
a `[floor.chat]` table. The daemon ignores it and logs a warning; delete the table to silence
the warning.

Restarting stops running agent turns. Claude Code and Pi stream their replies. Codex streaming
is an optional mode for Codex CLI **0.160.x** (any 0.160 patch release) on macOS and Linux:

```sh
dudes config set providers.codex on providers.codex.streaming on
dudes quickstart --restart
dudes login codex
dudes open
```

This mode keeps its own Codex profile under the daemon's state directory. Sign in once through
`dudes login codex`; Dudes does not copy credentials from your usual Codex profile. Other Codex
versions and managed enterprise configurations are refused in streaming mode. Leave
`providers.codex.streaming` off to use the existing Codex execution mode, which shows the reply
after completion. Windows currently supports that execution mode only.

Chat retains each agent's draft and shows queued messages, live replies, failures and any
history or reply limits. If the provider cannot resume a conversation, Chat asks you to confirm
a fresh conversation before sending again. An agent's structured question has its own **Answer**
action, which you turn on in quickstart.
