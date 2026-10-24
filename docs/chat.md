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

For an agent that Dudes launches with Claude Code, Codex or Pi, Chat lists the steps of the agent's latest
turn above its reply. A step is a command it ran, a file it read, edited or wrote, a search, a web request,
a plan, a call to a connected server, or the work of a helper agent. Each step is one line with its status
(running, done or failed) and how many bytes of output it produced. Open a step to see its inputs and, for
an edit, the old and new text. The Crew list shows the kind of step an agent is in, such as "Running a
command".

The three engines report different things:

- **Claude Code** reports all of the above. A helper agent's steps are nested under the step that started it.
- **Codex** reports commands, file changes, web searches, plans and connected-server calls. It gives no
  edit text, no result for a file change or a search, and no parent for a nested step. A file change shows
  the file's path, and a rename shows where the file moved to.
- **Pi** reports commands, reads, edits, writes, searches and connected-server calls. It gives no exit
  status and no parent, so a step is marked failed only when Pi says the tool call failed.

The step list and the text inside it are kept in memory only. They are not in the journal, and a restart
clears them. What Dudes does save is a count for each turn, written when the turn ends: how many commands,
reads, edits and so on the turn took, and how many files its edits named. The count holds no names or
text. A ticket shows it as "Engine-reported steps".

### Step output

A finished step's output is not sent with the step. Open the step and choose **Show the output**, and Dudes
fetches that one step's output for the agent you have selected. Before the output leaves Dudes, every known
secret is replaced with `(redacted)`: the agent tokens, the operator credential, secrets for connected
servers, and secret-looking values in the environment of any running agent. The output is then cut at
8 KiB, and a cut output says so. A step that is still running has no output yet, and after a restart the
page says the text is no longer kept.

Dudes shows the output of commands, reads, searches, web requests and helper agents. Some output is hidden,
and the page gives the reason:

- The step names a sensitive path, such as `~/.ssh`, `~/.aws`, a `.env` file or a key file. It still shows
  what it ran, but its output and its edit text stay hidden.
- A line of the output comes from a sensitive file, as in search results that start with `.env:`.
- The output holds a value that looks like a secret, such as a token or a `PASSWORD=` line.
- The step is a call to a connected server (MCP) or to a tool Dudes does not know.
- The step reads a background shell's later output.

Each step input is cut at 2 KiB and marked.

### What redaction cannot catch

Redaction matches a secret exactly as it is written in the text. A secret that is split across lines,
encoded in an unusual way, or never known to Dudes can still appear. For example, a command can print the
contents of a file that holds a secret Dudes never learned. Anyone holding a Floor session can already ask
an agent to print such a file, but a screen share or an unlocked browser shows the steps without being asked.
The Attach terminal shows the agent's own terminal output and is not scrubbed.

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
