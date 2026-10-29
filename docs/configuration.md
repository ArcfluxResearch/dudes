# Configuration

Dudes reads its settings from one operator file when the daemon starts. The Floor's `floor.*`
switches also take effect on a running daemon, with no restart, when you change them with
`dudes config set`. Every other change needs a restart (`dudes quickstart --restart`).

## The operator file

The operator file is `~/Library/Application Support/dudes/config.toml` on macOS,
`$XDG_CONFIG_HOME/dudes/config.toml` or `~/.config/dudes/config.toml` on Linux, and
`%APPDATA%\dudes\config.toml` on Windows. `DUDES_CONFIG` replaces the platform path with an
absolute one. A relative or empty value is refused, so a typo cannot make Dudes read a different
file. The daemon and `dudes` both read the variable.

`quickstart` keeps the file's other settings and comments where it can. The daemon writes one line
to `duded.log` in its runtime directory (`~/.local/state/dudes` by default) with its version,
socket, operator file and Floor address.

## Change settings

`dudes config` prints the file's path and each setting it manages. `dudes config set <key> <value>`
and `dudes config unset <key>` edit it, and one write carries every key given. For example:

```sh
dudes config set providers.codex on floor.hire on
```

`unset` removes a key, so the daemon's default applies. An edit that breaks the file's own start
rules is refused and names the key it needs. With a daemon running, an edit to a `floor.*` switch
applies at once and the command says so. Turning `floor.terminal` off closes open terminals at once.
A setting the daemon reads only at start, such as an engine, is named for a restart: `dudes stop`, then
`dudes start`.

- `providers.codex` and `providers.pi`, on or off, turn on those engines. Claude Code is always on.
- `providers.codex.streaming` opts in to Codex streaming with a separate sign-in home. It defaults
  to off and requires macOS or Linux. After restarting, sign in with `dudes login codex`. See
  [Chat](chat.md).
- `floor.answer`, `floor.hire`, `floor.control`, `floor.recovery`, `floor.goal`, `floor.renew`,
  `floor.retire`, `floor.board`, `floor.project`, `floor.terminal`, `floor.git` and `floor.diff`,
  on or off, are the Floor's actions. Each is off unless you turn it on, and `dudes quickstart`
  turns on every one except `floor.terminal` in a file that turns no Floor action on yet. `floor.project` lets the Floor add and remove a
  project, and a project with an agent working in it is not removed. It also lets the Floor open your
  system's folder window through the daemon, on macOS and on Linux with a display; a daemon started over
  SSH, or on Windows, offers no window and the folder is typed. Chat is always on and has no setting. `floor.board`
  lets the Floor make tickets and give them to agents, which then run and spend under your account,
  so it needs `floor.control` and `floor.goal` on. `floor.diff` lets the Floor show the files a
  ticket's agent changed, so it only has an effect with `floor.git` on. See
  [The Floor](floor.md#tickets).
- `budget.floor_usd_per_day` and `budget.per_turn_floor.usd` are in dollars, and
  `budget.per_turn_floor.tokens` is in tokens. See [Budgets](#budgets).

Recovery inspection and submission need both `floor.recovery` and `floor.control` on. **Check receipt** is read-only. With a live authenticated Floor reader, it remains available
when recovery is off while the current page retains the request. After reloading the page, use
`dudes recovery receipt <request-id>` with the saved ID. Receipt lookup never resubmits recovery.
See [Recover an interrupted goal](goals-and-stopping.md#recover-an-interrupted-goal).

An edit that takes a per-turn reserve to zero is written with a warning, because the daemon refuses
to start while an agent it has hired has a ceiling in that unit.

`floor.project` also takes `create_parent`, an absolute folder or one starting with `~/`, which you
edit in the file: it has no `dudes config` key, and the daemon reads it at start, so change it and
restart. With it set, the Add project dialog offers **Create a new project**. You type a name of 1
to 64 letters, digits, `.`, `_` or `-`, starting with a letter or digit. Dudes makes that folder
inside `create_parent` with mode `0700`, starts a git repository in it on the branch `main` with one
empty commit named `Initialize project` by `Dudes <dudes@localhost>`, and adds it as a project with
the "files changed" check. That needs git 2.28 or newer, on macOS and Linux. The folder must not
exist yet, and neither `create_parent` nor any folder above it may be a symbolic link. Creating a
project does not trust it and starts no agent. Once Dudes begins to change this file for the new
project it never deletes the folder, even when it undoes the change, and it tells you the exact path
that is left.

```toml
[floor.project]
enabled = true
create_parent = "~/projects"
```

MCP servers are declared in the same file. See [Give an agent MCP servers](mcp-servers.md).

## Projects with explicit repositories

On macOS and Linux, an existing project can configure an ordered list of 1 to 8 plain repository
checkouts. Linked worktrees, submodules, and symlinked `.git` directories are refused. Starting or
reloading the daemon validates the list in memory without recording a repository catalog.
Per-repository trust for a newly configured list and scoped execution are unavailable in this version.
These projects cannot be hired into from the CLI or Floor, including by passing one member's folder
with `--cwd`.

Replace a project's complete repository list in one edit:

```sh
dudes project repositories service --primary web \
  --repo web=/absolute/path/to/web --repo api=/absolute/path/to/api \
  --repo docs=/absolute/path/to/docs
```

The command edits the existing project and asks the daemon to adopt the change. It creates no
folder or Git repository. The 32-project limit counts projects, independently of their member count.
Each repository id starts with a lowercase letter and uses at most 32 lowercase letters, digits,
underscores, or hyphens. A recorded id cannot later name another origin. Duplicate directories,
overlapping roots, and shared Git common directories are refused.

The operator file uses `primary_repo` and ordered `repositories` instead of `cwd`. In an existing
project entry, replace its `cwd` line with:

```toml
primary_repo = "web"
repositories = [
  { repo_id = "web", label = "Web", path = "/absolute/path/to/web" },
  { repo_id = "api", label = "API", path = "/absolute/path/to/api" },
  { repo_id = "docs", label = "Docs", path = "/absolute/path/to/docs" },
]
```

Keep the entry's checks and other settings. `cwd` and the explicit list cannot appear together.
Trust remains an operator action and is never read from configuration. An existing recorded catalog
requires `--repo` when trusting or revoking a member. The existing settings inspection and refusals
apply to each grant. Removing a member revokes its grant without deleting any checkout.

Expanding a project pauses agents hired under its original singleton authority. Their old tickets,
queued messages, and interrupted turns cannot gain access to new members. Resume stays refused
while current configuration is expanded or unresolved. A malformed reload or a refused remap holds
recorded project work until the operator adopts valid configuration. Restoring the original singleton
allows its original authority again, subject to the normal pause and recovery consent rules.
Repository removal is refused while live hires or unresolved tickets still reference it.

An unchanged legacy `cwd` entry keeps its existing singleton hire and trust behavior.

A named `[[floor.onboarding.workspaces.checks]]` row can also store `check_repo = "api"`.
The value is a repository id from that project's configured list, never a path. An unknown id
is refused. Without the field, the check keeps the project's primary repository. A legacy `cwd`
project has only the repository id `primary`.

This field prepares the configured target for scoped tickets. Target-directed check execution
is unavailable in this version. A future scoped ticket must include that configured repository
in its declaration. Clients cannot redirect the check to another repository or provide its path.

## Budgets

Budgets are optional, and none is set by default. An agent with no ceiling still stops at its
goal's hop cap, and the engine's own plan limits apply to it. A ceiling you set is checked
before every turn: `budget.floor_usd_per_day` caps the whole floor's dollars per day,
`dudes project add` takes optional ceilings for the agents and goals the Floor hires there,
and `dudes hire` and `dudes goal` take `--usd-per-day`, `--tokens-per-day`, `--usd` and
`--tokens`. A turn whose usage the daemon never saw counts against a set ceiling at the per-turn
reserve: $0.25 and 20,000 tokens, unless you set `budget.per_turn_floor.usd` or
`budget.per_turn_floor.tokens`. When the daemon's own environment holds an engine's API key, a
hire or goal on that engine needs a ceiling: any ceiling for Claude Code, and a token ceiling for
Codex and Pi, which report no dollars.

## Environment variables

- `DUDES_CONFIG` is the operator file, as an absolute path.
- `DUDES_STATE_DIR` is where the daemon keeps its journal and projections. The default is
  `~/.local/share/dudes`.
- `DUDES_RUNTIME_DIR` is where the daemon puts its socket and operator credential. The default is
  `~/.local/state/dudes`.
- `DUDES_SOCK` is the control socket. The default is `duded.sock` in the runtime directory.
- `DUDES_DAEMON` is the daemon executable. The default is a `duded` beside the `dudes` binary.
- `DUDES_FLOOR_ASSETS` and `DUDES_FLOOR_PORT` choose the Floor bundle and port. See
  [Serve the Floor](floor.md#serve-the-floor).

On Windows with `HOME` unset, the default directories sit under `%USERPROFILE%` instead of `HOME`.
See [Where Dudes keeps its data](data-and-uninstall.md#where-dudes-keeps-its-data).
