# Get started

This walk-through takes you from a fresh install to an agent working on a goal. It uses
`dudes quickstart`, which sets everything up for you: the engines, the Floor's actions, the
repository the Floor hires into and a first agent.

You need Dudes [installed](../README.md#install) and at least one of the `claude`, `codex` and `pi`
CLIs. `quickstart` tells you whether each is signed in and prints the command to sign in.
Dudes reads no engine's credentials or files.

1. Check that `dudes --version` runs. If the installer said that `~/.local/bin` is not on your `PATH`,
   add the line it printed to your shell profile and open a new shell.

2. From the repository you want agents to work in, run the guided quickstart:

   ```sh
   cd /absolute/path/to/repo
   dudes quickstart
   ```

   On a terminal, `quickstart` works out what it can and asks the rest one step at a time. Enter
   takes the answer shown in capitals or in brackets. Pressing Enter at every question gives a
   setup. The guide asks at most five questions, including the final confirmation:

   - **Engines (no question).** Claude Code is always on. Codex and Pi are turned on when their
     program is on your `PATH`, and left as your file has them when it is not. Codex streaming is
     never asked about; see [Chat](chat.md) to opt in.
   - **Floor actions (one question).** Let the Floor hire, pause and resume, answer, change goals,
     give tickets to agents, retire and attach a terminal. Yes turns all seven on and No turns them
     off. A ticket given to an agent starts it, and agents then run and spend under your account.
     Anyone holding your one-time Floor link could start agents that act with your account's full
     reach. When your
     file already turns on only some of them, quickstart keeps that choice and does not ask.
     Starting a new goal when an agent's goal has nothing left stays as your file has it, and
     is off in a new file.
   - **Spending cap (only with an API key).** Asked only when your shell holds an API key for an
     engine that is on. Subscription plans have their own limits. See
     [Budgets](configuration.md#budgets).
   - **Project.** When the current directory is a git repository that is not enrolled, one
     question for its completion check, the command the daemon runs itself to decide that a goal
     is done. Quickstart suggests `cargo test`, `npm test`, `go test ./...`, `pytest` or
     `make test` from the files it finds, and when it finds none it offers a check that passes once the
     agent has edited or added a file; `-` skips enrolling. Quickstart marks the check as one that
     may already pass, so the Floor's Hire does not refuse a project whose tests already pass. Outside a repository with no
     project enrolled, the guide asks for a project folder instead. Enter skips it.
   - **First hire.** With a project, the guide offers an idle agent named Kim. Confirm the only
     installed engine, or choose an engine when several are installed. Enter skips the engine
     choice. The agent has no goal and starts no work. Use quickstart's explicit hire, goal and
     budget options for a different name, a goal or agent limits.

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
   - prints `dudes login claude` or `dudes login codex` for each engine that is signed out.
     Run the command to sign in.

   If you skipped the project, tickets and Floor hiring need one before they can work. Add it
   from its git repository:

   ```sh
   cd /absolute/path/to/repo
   dudes workspace add
   ```

   The command uses a unique slug of the folder name as the project id, the suggested test
   command as the check and its label, department `engineering`, a five-turn hop cap, and a
   600-second check timeout. It prints what it enrolled and how to change it. If the repository
   suggests no check, pass one with `dudes workspace add -- <program> [args...]`. Explicit flags
   override the defaults. A completion check must fail until the particular goal is met.

   The operator file and the `duded.log` line that `quickstart` leaves are described in
   [Configuration](configuration.md#the-operator-file).

3. Paste the link into a browser and use the Floor. The link works once, in one browser;
   `dudes floor open` prints a new one. On a remote machine, forward the link's port over SSH first, because
   the link is a loopback address. [The Floor](floor.md) explains how it signs in, what each tab shows and what an agent's position
   in the office means.

4. Give an agent a ticket from the Floor. Open **Board**, press **New ticket**, type a title and
   choose **Create ticket**. Open the card and choose **Give to agent**. When the agent's check
   passes the ticket moves to **Ready for you**, and the top bar says so. Read what the agent
   says it did and the files it changed, then choose **Accept**, or **Ask for changes** with a
   message. See [Tickets](tickets.md).

5. Hire more agents. On the Floor, click **Hire an agent**, choose the workspace and an engine it
   offers, and write the briefing. The Floor offers Claude Code, and Codex or Pi once they are
   turned on. From the command line, hire an agent, then give it a goal with the id that `hire`
   prints. This one gets a folder of its own. Dudes cannot check another agent's engine settings
   for a ticket, so in a repository two agents share it holds tickets in **Needs you** for review:

   ```sh
   mkdir -p ~/dudes-hello
   dudes hire --name Cody --role "docs writer" --department docs --provider claude \
     --cwd ~/dudes-hello
   dudes goal --agent <agent id> --text "Create hello.txt that contains the word hello." \
     --hop-cap 2 --check-id hello --check-label "hello.txt exists" -- /bin/test -f hello.txt
   dudes agents
   ```

   Use `--provider codex` or `--provider pi` instead if that is the CLI you have signed in.

   `--check-id` and `--check-label` name the check. The goal records that name and a revision of
   the check, and the Floor shows them in place of the command.

   That folder is not an enrolled project, so `dudes hire` warns that Cody can't take tickets; the
   goal does not need one. Omit `--cwd` to use the only enrolled project or choose one on a terminal
   when several exist. With no project, the CLI warns the same way. To give a second agent in the
   same repository a tree of its own instead, hire a Claude Code agent with `--worktree` in a
   repository you mark with `--trusted`.

   The role is a label. Dudes shows it on the Floor, in `dudes agents` and in `dudes team`, which
   agents use to find colleagues. It does not put the role in the agent's own task, so describe
   the work in the goal or in a chat message.

   The agent takes turns until the daemon's own run of the completion check passes, a ceiling or
   hop cap stops it, or a launch failure puts it on Hold. Two fast failed turns without reported
   usage also put it on Hold. Fix the cause and resume it to continue. `dudes agents` lists every
   hired agent and its status. [Goals and stopping](goals-and-stopping.md) covers replacing a goal,
   pausing, halting and retiring an agent.

6. Stop the daemon and the agent processes it started:

   ```sh
   dudes stop
   ```

   `stop` gives each agent a short closing time to save its work. See
   [Stop Dudes](goals-and-stopping.md#stop-the-daemon-and-closing-time).

   To start the daemon again, for example after you rerun the installer instead of using `dudes upgrade`, run
   `dudes start`. It asks nothing, changes no settings and hires no agent. If a daemon is
   already running it says so, and it tells you when that daemon is a different version from
   `dudes`.

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

Claude Code is always on. Quickstart turns Codex and Pi on when it finds their programs on your
`PATH`. Otherwise turn one on with `dudes config set providers.codex on` (or `providers.pi`),
followed by `dudes quickstart --restart`.

## Run quickstart without the guide

Without a terminal and without options, `dudes quickstart` only starts the daemon and prints its
version and control socket, plus a missing-project hint unless `--json` is set. With options it asks nothing,
prints no live Floor link: it prints `dudes floor open`, and `dudes login
<engine>` for each engine that is signed out. `--yes` takes every default, including turning on the Floor's hire, goal, control, answer and board actions when the file turns none on (terminal, renew, retire and the git and diff views stay off), `--set <key> <value>`
sets what `dudes config set` sets, `--enroll` enrolls the current repository and `--hire <name>`
hires a first agent there, with the completion check after `--`; a setting that breaks one of the
file's start rules is refused by name, and nothing is written. A script restarts a running daemon
with other settings only with `--restart`; without it, `quickstart` writes the file, names what
differs, stops nothing and exits 4. To set things one at a time instead, use `dudes config set` (for
example `dudes config set providers.codex on floor.hire on`), `dudes workspace add` and `dudes
hire`, and restart the daemon with `dudes quickstart --restart` so it reads the file. `dudes config`
prints the file's path and each setting it manages, and refuses an edit that breaks one of the
file's start rules, naming the missing key.
