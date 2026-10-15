# Configuration

Dudes reads its settings from one operator file when the daemon starts. A change needs a restart
(`dudes quickstart --restart`).

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
rules is refused and names the key it needs. With a daemon running, an edit names the settings it
runs differently, which a restart applies: `dudes stop`, then `dudes start`.

- `providers.codex` and `providers.pi`, on or off, turn on those engines. Claude Code is always on.
- `providers.codex.streaming` opts in to Codex streaming with a separate sign-in home. It defaults
  to off and requires macOS or Linux. After restarting, sign in with `dudes login codex`. See
  [Chat](chat.md).
- `floor.answer`, `floor.hire`, `floor.control`, `floor.goal`, `floor.renew`, `floor.retire`,
  `floor.board`, `floor.terminal`, `floor.git` and `floor.diff`, on or off, are the Floor's
  actions. Each is off unless you turn it on. Chat is always on and has no setting. `floor.board`
  lets the Floor make tickets and give them to agents, which then run and spend under your account,
  so it needs `floor.control` and `floor.goal` on. `floor.diff` lets the Floor show the files a
  ticket's agent changed, so it only has an effect with `floor.git` on. See
  [The Floor](floor.md#tickets).
- `budget.floor_usd_per_day` and `budget.per_turn_floor.usd` are in dollars, and
  `budget.per_turn_floor.tokens` is in tokens. See [Budgets](#budgets).

An edit that takes a per-turn reserve to zero is written with a warning, because the daemon refuses
to start while an agent it has hired has a ceiling in that unit.

MCP servers are declared in the same file. See [Give an agent MCP servers](mcp-servers.md).

## Budgets

Budgets are optional, and none is set by default. An agent with no ceiling still stops at its
goal's hop cap, and the engine's own plan limits apply to it. A ceiling you set is checked
before every turn: `budget.floor_usd_per_day` caps the whole floor's dollars per day,
`dudes workspace add` takes optional ceilings for the agents and goals the Floor hires there,
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
