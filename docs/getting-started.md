# Get started

This walk-through takes you from a fresh install to an agent working on a goal. It uses
`dudes quickstart`, which sets everything up for you: the engines, the Floor's actions, the
repository the Floor hires into and a first agent.

You need Dudes [installed](../README.md#install) and at least one of the `claude`, `codex` and `pi`
CLIs. `quickstart` tells you whether each is signed in and can run an engine's own login for you.
Dudes reads no engine's credentials or files.

1. Check that `dudes --version` runs. If the installer said that `~/.local/bin` is not on your `PATH`,
   add the line it printed to your shell profile and open a new shell.

2. From the repository you want agents to work in, run the guided quickstart:

   ```sh
   cd /absolute/path/to/repo
   dudes quickstart
   ```

   On a terminal, `quickstart` asks one step at a time, and Enter takes the answer shown in
   capitals or in brackets:

   - **Engines.** Claude Code is always on; turn on Codex and Pi if you use them. On macOS and
     Linux, you can also opt in to Codex streaming, which uses a separate sign-in.
   - **Floor actions.** Answering an agent's question, hiring an agent, pausing,
     resuming and renaming an agent, changing its goal, starting a new goal when an agent's goal
     has nothing left (this needs goal change and Hire), retiring it and attaching a terminal.
     Each is off unless you turn it on.
   - **Spending cap.** Skipped unless you choose it. Subscription plans have their own limits; set
     a daily dollar cap for the whole floor when an engine bills an API key for each token. See
     [Budgets](configuration.md#budgets).
   - **Workspace.** Enroll the current repository for the Floor's Hire, with the completion check
     the daemon runs itself to decide that a goal is done.
   - **First hire.** Optionally hire an agent in the current directory, with a goal or without one.
     An agent with a goal starts working at once.

   After a summary you confirm, `quickstart` writes the daemon's operator file once and starts the
   daemon. When one is already running with other settings, the guide says so before you confirm,
   because the restart ends any turn it is running. The running daemon first reads the new file, and
   a file it would refuse is put back as it was, with nothing stopped. Then `quickstart`:

   - prints the daemon's version and control socket;
   - prints each engine it turned on, where the daemon found that engine's program on its `PATH`,
     and whether it is signed in. The daemon asks the engine's own status command (`claude auth
     status` or `codex login status`) and reads only its exit status. Pi has no such command:
     run `pi`, then type `/login` inside it;
   - prints a one-time link to the Floor, as `dudes floor open` does, for you to paste into any
     browser;
   - prints the hired agent's id, when you hired one;
   - last, when an engine is signed out, names the others that are and offers to sign in to one.
     A yes runs that engine's own login in your terminal, and `quickstart` ends there.
     `dudes login claude` or `dudes login codex` does the same later.

   The operator file and the `duded.log` line that `quickstart` leaves are described in
   [Configuration](configuration.md#the-operator-file).

3. Paste the link into a browser and use the Floor. The link works once, in one browser;
   `dudes floor open` prints a new one. [The Floor](floor.md) explains how it signs in, what each tab shows and what an agent's position
   in the office means.

4. Hire more agents. On the Floor, click **Hire an agent**, choose the workspace and an engine it
   offers, and write the briefing. The Floor offers Claude Code, and Codex or Pi once they are
   turned on. From the command line, hire an agent, then give it a goal with the id that `hire`
   prints:

   ```sh
   dudes hire --name Cody --role "docs writer" --department docs --provider claude \
     --cwd /absolute/path/to/repo
   dudes goal --agent <agent id> --text "Create hello.txt that contains the word hello." \
     --hop-cap 2 --check-id hello --check-label "hello.txt exists" -- /bin/test -f hello.txt
   dudes agents
   ```

   `--check-id` and `--check-label` name the check. The goal records that name and a revision of
   the check, and the Floor shows them in place of the command.

   The role is a label. Dudes shows it on the Floor, in `dudes agents` and in `dudes team`, which
   agents use to find colleagues. It does not put the role in the agent's own task, so describe
   the work in the goal or in a chat message.

   The agent takes turns until the daemon's own run of the completion check passes, a ceiling or
   hop cap stops it, or a launch failure puts it on Hold. Two fast failed turns without reported
   usage also put it on Hold. Fix the cause and resume it to continue. `dudes agents` lists every
   hired agent and its status. [Goals and stopping](goals-and-stopping.md) covers replacing a goal,
   pausing, halting and retiring an agent.

5. Stop the daemon and the agent processes it started:

   ```sh
   dudes stop
   ```

   `stop` gives each agent a short closing time to save its work. See
   [Stop Dudes](goals-and-stopping.md#stop-the-daemon-and-closing-time).

`dudes --help` lists every command and option.

## Troubleshooting

### The daemon refuses to start because a Floor directory is group-writable

On Unix, the daemon refuses to start when the installed Floor, or one of the directories above it
(`~/.local/share/dudes`, `~/.local/share` or `~/.local`, for the default prefix), can be written by
its group or by others. Common Linux umasks create a directory that way. The error names the
directory and says to run `chmod go-w` on it. That is the fix:

```sh
chmod go-w <the directory the error names>
```

### Hiring with Codex or Pi is refused

Claude Code is always on. Codex and Pi are off until you turn them on, in the quickstart's
**Engines** step or with `dudes config set providers.codex on` (or `providers.pi`), followed by
`dudes quickstart --restart`.

## Run quickstart without the guide

Without a terminal and without options, `dudes quickstart` only starts the daemon and prints its
version and control socket, as scripts expect. With options it asks nothing,
prints no live Floor link: it prints `dudes floor open`, and `dudes login
<engine>` for each engine that is signed out. `--yes` takes every default, `--set <key> <value>`
sets what `dudes config set` sets, `--enroll` enrolls the current repository and `--hire <name>`
hires a first agent there, with the completion check after `--`; a setting that breaks one of the
file's start rules is refused by name, and nothing is written. A script restarts a running daemon
with other settings only with `--restart`; without it, `quickstart` writes the file, names what
differs, stops nothing and exits 4. To set things one at a time instead, use `dudes config set` (for
example `dudes config set providers.codex on floor.hire on`), `dudes workspace add` and `dudes
hire`, and restart the daemon with `dudes quickstart --restart` so it reads the file. `dudes config`
prints the file's path and each setting it manages, and refuses an edit that breaks one of the
file's start rules, naming the missing key.
