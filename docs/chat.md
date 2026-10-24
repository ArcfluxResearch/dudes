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
action, which quickstart turns on in a new file.

## Steps

For a Claude Code agent that Dudes launches, Chat lists the steps of the agent's latest turn above its
reply: the commands it ran, the files it read, edited or wrote, its plan, and the work of any helper agent,
nested under the step that started it. Each step is one line. Open it to see its inputs and, for an edit, the
old and new text. A step says whether it is running, done or failed, and how many bytes of output it produced.
The output itself is not sent to the browser. Other agents show only the kind of step they are in, such as
"Running a command", in the Crew list.

Steps are shown as plain text, live, and are never saved: they are not in the journal, and a restart
clears them. Before a step leaves the daemon, every known secret is replaced with `(redacted)`: the
agent tokens, the operator credential, secrets for connected servers, and secret-looking values in the
environment of any running agent. A step that names a sensitive path (`~/.ssh`, `~/.aws`, a `.env` file, a
key file) still shows what it ran, but its edit text stays hidden, and so does any input that looks like a
secret. Each input is cut at 2 KiB and marked.

This cannot catch everything. A command can print a secret that Dudes never learned, for example the
contents of a file passed to `curl`, and if its shape is not one of those above it appears on screen. Anyone
holding a Floor session can already ask an agent to print such a file, but a screen share or an unlocked
browser shows the steps without being asked. The Attach terminal shows the agent's own terminal output and
is not scrubbed.

## Sending files

You can send an agent a screenshot, a log or a short spec three ways: paste an image or file from the
clipboard into the message box, drop files on the message box, or press **+** and pick files. Each file
shows as a chip with its name and a **Remove** button. A file uploads as soon as you attach it, so a
file that is too big or a full folder shows its error on the chip at once. Press Send and the message
carries a short list after your text:

```
Please look at this screenshot.

Attached files:
- /path/to/the/copy.png (shot.png)
```

The agent opens the file by that path. A message may be only files.

- **Where the copy lives.** The Floor sends the file's bytes, never its path on your machine. The
  daemon keeps a copy in the agent's own `files` folder under the state directory, named by 16 random
  hex digits and a cleaned version of the file name (letters, digits, `.`, `_` and `-` only). The daemon
  never opens or runs the file, and your original is not touched.
- **Limits.** A file can be up to 10 MiB. An agent's `files` folder holds up to 200 MiB. When it is full
  the chip shows the folder's path so you can delete files from it. The message text, with the list of
  paths, still has the usual 4096-byte limit; Send is blocked, with a message, if it would pass it.
- **Cleanup.** The agent's `files` folder is deleted when the agent retires.
- **Which agents.** Files work with agents Dudes launches. For an Attached agent, whose engine Dudes
  does not launch, the **+** button is hidden and pasted or dropped files are ignored. Claude is given
  read access to the folder when it starts a turn; Codex already reads it.
