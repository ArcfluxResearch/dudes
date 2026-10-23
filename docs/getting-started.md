# Get started

This walk-through takes you from a fresh install to an agent working on a goal. It uses
`dudes quickstart`, which sets up the engines and the Floor's actions, starts the daemon and opens
the Floor, where you add a project and hire.

You need Dudes [installed](../README.md#install) and at least one of the `claude`, `codex` and `pi`
CLIs. `quickstart` tells you whether each is signed in and prints the command to sign in.
Dudes reads no engine's credentials or files.

1. Check that `dudes --version` runs. If the installer said that `~/.local/bin` is not on your `PATH`,
   add the line it printed to your shell profile and open a new shell.

2. Run quickstart:

   ```sh
   dudes quickstart
   ```

   Quickstart asks no questions. It does this, in order:

   - **Engines.** Claude Code is always on. Codex and Pi are turned on when their program is on
     your `PATH`, and left as your file has them when it is not. Codex streaming stays as it is;
     see [Chat](chat.md) to opt in.
   - **Floor actions.** On a file that turns no Floor action on yet, it turns on answer, hire,
     control, goal, retire, board, renew, recovery and project, so you can hire, pause and resume,
     answer, change goals, give tickets to agents, retire, recover and add projects from the
     Floor. A ticket given to an agent starts it, and agents then run and spend under your
     account. Anyone holding your one-time Floor link could start agents that act with your
     account's full reach. The changed-file views (`floor.git` and `floor.diff`) are on too, so
     a ticket lists the files its agent changed and opens each one side by side. The terminal
     stays off. When your file already turns any Floor action on, quickstart keeps
     every Floor setting as it is.
   - **No cap, project or agent.** It sets no spending cap (subscription plans have their own
     limits; see [Budgets](configuration.md#budgets)), enrolls no project and hires no agent. You
     add a project and hire from the Floor.

   Then `quickstart` writes the daemon's operator file once, with no confirmation, and lists each
   change it made. It starts the daemon. When one is already running, it adopts the Floor
   switches and project changes at once. A setting the daemon reads only when it starts needs a
   restart, and quickstart restarts the daemon only when no agent is running a turn, waiting for
   an answer or attached to a person. Otherwise it stops nothing, names the busy agents and the
   commands that restart the daemon, and exits 4. The running daemon first reads the new file, and
   a file it would refuse is put back as it was, with nothing stopped. Then `quickstart`:

   - prints the daemon's version and control socket;
   - prints each engine it turned on, where the daemon found that engine's program on its `PATH`,
     and whether it is signed in. The daemon asks the engine's own status command (`claude auth
     status` or `codex login status`) and reads only its exit status. Pi has no such command:
     run `pi`, then type `/login` inside it;
   - prints `dudes login claude` or `dudes login codex` for each engine that is signed out.
     Run the command to sign in;
   - opens the Floor in your default browser with a one-time link, as `dudes open` does (over SSH,
     or where no opener is found, it prints the link for you to paste into a browser).

   Quickstart enrolls no project and hires no agent for you, and the Floor does both.

   Tickets and Floor hiring need a project before they can work. On the
   Floor, press Choose folder… to pick it in your system's folder window (the button shows when Dudes
   can open one), or type it into the "Folder on this computer" box. Look at what Dudes
   found, and press Add project. The folder must be a git repository, and it cannot be your home
   folder, a hidden folder or one of Dudes' own. Or add it from a terminal, in its git repository:

   ```sh
   cd /absolute/path/to/repo
   dudes project add
   ```

   The command uses a unique slug of the folder name as the project id, the suggested test
   command as the check and its label, department `engineering`, a five-turn hop cap, and a
   600-second check timeout. It prints what it enrolled and how to change it. If the repository
   suggests no check, pass one with `dudes project add -- <program> [args...]`. Explicit flags
   override the defaults. A completion check must fail until the particular goal is met.

   The operator file and the `duded.log` line that `quickstart` leaves are described in
   [Configuration](configuration.md#the-operator-file).

3. Use the Floor in the browser that opened. The link works once, in one browser;
   `dudes open` (or the Dudes launcher that the installer adds) opens a new one. On a remote machine, `dudes open` prints the link; forward its port over SSH
   first, because the link is a loopback address. [The Floor](floor.md) explains how it signs in, what each tab shows and what an agent's position
   in the office means.

4. Hire an agent on the Floor once your project is added: click **Hire an agent**, name it, choose the
   project and an engine. A hire needs no task; the agent waits, idle, at its desk. The Floor then
   offers **Give a task** for it: type what it should do and choose **Start**, or open **Board**,
   press **New ticket**, type a title and choose **Create ticket**, then open the card and choose **Give to agent**. When the agent's check
   passes the ticket moves to **Ready for you**, and the top bar says so. Read what the agent
   says it did and the files it changed, then choose **Accept**, or **Ask for changes** with a
   message. See [Tickets](tickets.md).

5. Hire more agents. The Floor offers Claude Code, and Codex or Pi once they are
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

## Install a release

The [install command](../README.md#install) downloads the latest release for your machine and
checks it against the release's `SHA256SUMS` before it installs anything. It never uses `sudo`
or edits your shell profile. If the programs are not on your `PATH`, it prints the line to add
to your shell profile. See [Where Dudes keeps its data](data-and-uninstall.md#where-dudes-keeps-its-data)
for the installed files and their locations.

To install a specific version, pass its version to the script:

```sh
curl -fsSL https://raw.githubusercontent.com/ArcfluxResearch/dudes/main/install.sh | sh -s -- --version <version>
```

For a source build or Windows, see [Build Dudes from source](building-from-source.md).
To remove Dudes, see [Where Dudes keeps its data, and how to uninstall](data-and-uninstall.md).

## Upgrade Dudes

To check for a new release without changing anything, run:

```sh
dudes upgrade --check
```

To install the latest release and restart the daemon, run:

```sh
dudes upgrade
```

The command uses the release's installer, which checks the download against `SHA256SUMS`.
It works only for a copy that `install.sh` installed and never installs an older release.
It refuses while an agent is running a turn, waiting for an answer, or attached to your session.
Add `--now` to restart with closing time, which gives agents up to 6 minutes to save their work.
Dudes checks for updates only when you run the command.

You can also run the [install command](../README.md#install) again, then run `dudes stop` and
`dudes start`. See [After you upgrade](floor.md#after-you-upgrade) for browser sign-ins and reloads.

The first upgrade from a version that gave the page a readable secret signs open tabs out once; run `dudes open` to sign in again.

## Troubleshooting

### The daemon refuses to start because a Floor directory is group-writable

On Unix, the daemon refuses to start when the installed Floor, or one of the directories above it
(`~/.local/share/dudes`, `~/.local/share` or `~/.local`, for the default prefix), can be written by
its group or by others. Common Linux umasks create a directory that way. The error names the
directory and says to run `chmod go-w` on it. That is the fix:

```sh
chmod go-w <the directory the error names>
```

### The shell runs a different command named dudes

If `dudes` runs something else, check `type dudes` for an alias or shell function and use
`command dudes`. After removing an old definition from your shell profile, open a new terminal.

### Hiring with Codex or Pi is refused

Claude Code is always on. Quickstart turns Codex and Pi on when it finds their programs on your
`PATH`. Otherwise turn one on with `dudes config set providers.codex on` (or `providers.pi`),
followed by `dudes quickstart --restart`.

## Run quickstart from a script

Without a terminal and without options, `dudes quickstart` only starts the daemon and prints its
version and control socket, plus a missing-project hint unless `--json` is set. With options it
sets up as it does on a terminal, and opens the Floor the same way. `--yes` is accepted and
changes nothing. `--set <key> <value>` sets what `dudes config set` sets, after the defaults;
`--enroll` enrolls the current repository and `--hire <name>` hires a first agent there, with the
completion check after `--`. A setting that breaks one of the file's start rules is refused by
name, and nothing is written. `quickstart` restarts a daemon that is running a turn only with
`--restart`; without it, `quickstart` writes the file, names the busy agents, stops nothing and
exits 4. To set things one at a time instead, use `dudes config set` (for example `dudes config set
providers.codex on floor.hire on`), `dudes project add` and `dudes hire`, and restart the daemon
with `dudes quickstart --restart` so it reads the file. `dudes config`
prints the file's path and each setting it manages, and refuses an edit that breaks one of the
file's start rules, naming the missing key.
