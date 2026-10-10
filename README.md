# Dudes

[![CI](https://github.com/ArcfluxResearch/dudes/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/ArcfluxResearch/dudes/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/ArcfluxResearch/dudes)](https://github.com/ArcfluxResearch/dudes/releases/latest)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#licence)

Dudes is a supervisor for coding agents, written in Rust. You hire an agent, such as Claude Code,
Codex or Pi, and give it a role. You then talk to it, or give it a **goal** with a completion check.
A background daemon keeps the agent working until the daemon's own run of that
check passes. The **Floor** is a 3D office in your browser. It shows each agent at a desk, and what
you see there is what the daemon knows.

![The Floor in a browser. An isometric voxel office holds a crew of eight: four agents type at desks, two stand at the goal board between turns, one sits on the lounge sofa under a question mark, and one stands at the bell after its goal is done. The top bar counts the crew by status. The Crew list on the left shows each agent's initial, role, engine, workspace, goal and status. The drawer on the right is open on the waiting agent, with a portrait of its voxel figure, its question, an answer box and the buttons to pause, retire or attach to it.](.github/readme/floor.webp)

## Status

Dudes is an early preview (pre-alpha), and commands and configuration keys can still change between
releases. Prebuilt releases for macOS and Linux on x86_64 and arm64 install with one command, as
shown below. Windows can build the binaries but has no prebuilt release or installer. The full first run has been rehearsed only on macOS on Apple silicon. See
[Platforms and known limits](docs/platforms-and-limits.md).

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/ArcfluxResearch/dudes/main/install.sh | sh
~/.local/bin/dudes quickstart
```

The script downloads the latest release for your machine, checks it against the release's
`SHA256SUMS`, and only then installs `duded`, `dudes` and `dudes-hook` in `~/.local/bin`, and the
Floor and licence notices in `~/.local/share/dudes`. It never uses `sudo` and never edits your
shell profile. If `~/.local/bin` is not on your `PATH`, it prints the line to add to your shell
profile. Releases cover macOS and Linux on x86_64 and arm64. On Linux the script installs a static
build that runs on any distribution. `~/.local/bin/dudes --version` checks the install. To install
a specific version, run the script with `sh -s -- --version <version>`. You also need at least one of the `claude`,
`codex` or `pi` CLIs, signed in.

To build from source, or on Windows, see [Build Dudes from source](docs/building-from-source.md).
If the daemon later refuses to start because a Floor directory is writable by its group, run the
`chmod go-w` command that its error names (see [Get started](docs/getting-started.md#troubleshooting)).
To remove Dudes, see [Where Dudes keeps its data, and how to uninstall](docs/data-and-uninstall.md).

## Quick start

From the repository you want agents to work in:

```sh
cd /absolute/path/to/repo
dudes quickstart
dudes floor open
```

`dudes quickstart` asks only what it cannot work out, starts the daemon, prints a link to the Floor and can
hire a first agent. Paste the link into any browser; `dudes floor open` prints a new one later. On a remote
machine, forward the link's port over SSH first, because the link is a loopback address. To hire an agent
yourself and give it
a goal, use the id that `dudes hire` prints:

```sh
dudes hire --name Cody --role "docs writer" --department docs --provider claude \
  --cwd /absolute/path/to/repo
dudes goal --agent <agent id> --text "Create hello.txt that contains the word hello." \
  --hop-cap 2 --check-id hello --check-label "hello.txt exists" -- /bin/test -f hello.txt
```

The agent takes turns until `/bin/test -f hello.txt` passes. `dudes agents` lists every agent and
its status, and `dudes stop` stops the daemon. [Get started](docs/getting-started.md) is the full
walk-through.

## How it works

- **Daemon and CLI.** The `duded` daemon runs in the background and owns all state. The `dudes`
  command talks to it. The Floor is a view of what the daemon knows. It changes that state only
  through the actions you turn on.
- **Agents and roles.** An agent is one coding-agent CLI that the daemon hired to work in a
  directory. It has a name, a role and a department. The role is a label that the Floor, `dudes
  agents` and `dudes team` show. It is not part of the agent's task, so describe the work in the
  goal or in a chat message.
- **Goals and completion checks.** A goal is a piece of work plus a command, the completion check.
  Before the agent's first turn, the daemon runs the check once and refuses the goal if the check
  already passes. After each turn the daemon runs the check again. The goal is Done only when the
  check passes in the daemon's own run. What an agent reports about its own work never marks a goal
  Done.
- **Limits.** Spending ceilings are optional and off by default. A goal also stops at its hop cap,
  and a launch failure puts an agent on Hold. See [Goals, completion checks and
  stopping](docs/goals-and-stopping.md).
- **Mail.** Agents send each other messages with `dudes mail` and find colleagues with
  `dudes team`.
- **Worktrees.** `dudes hire --worktree` gives a Claude Code agent in a repository you mark with `--trusted` its own git worktree and a
  branch named `dudes/<id>`. Dudes never merges or pushes these branches.
- **The Floor.** An agent types at its desk during a turn, stands at the goal board while the
  daemon runs the check, waits in the lounge when it needs your answer, and rings the bell when the
  goal is Done. [The Floor](docs/floor.md) explains the rest, and [Chat](docs/chat.md) lets you
  message an agent from it.

## What agents can do on your machine

Dudes runs every agent and every completion check as your user account. It adds no sandbox of its
own. Before you hire an agent, know what each engine can reach:

- Claude Code runs with its permission prompts skipped (`--dangerously-skip-permissions`), and no
  Dudes setting turns them back on. It can run any command and read and change any file your
  account can.
- Codex runs inside its own `workspace-write` sandbox. Its commands can read any file your account
  can, and they write only in a few places the full text lists.
- Pi has no sandbox. It can run any command and read and change any file your account can.
- The completion check runs in the agent's working directory with no sandbox. Review what the agent
  changed, tests included, before you rely on a Done.

Your user account is not a security boundary. To limit what agents can reach, run Dudes in a
virtual machine or under a separate user account that holds only the repositories you want agents
to change. The [full text](docs/what-agents-can-do.md) has the details.

## Documentation

- [Get started](docs/getting-started.md): the full first run, from `dudes quickstart` to a goal.
- [The Floor](docs/floor.md): the office view, how it opens and how to serve it.
- [Chat](docs/chat.md): message an agent from the Floor, and Codex streaming.
- [Goals, completion checks and stopping](docs/goals-and-stopping.md): replace a goal, pause, halt,
  retire and stop.
- [Give an agent MCP servers](docs/mcp-servers.md): declare servers and opt agents in.
- [Configuration](docs/configuration.md): the operator file, settings, budgets and environment
  variables.
- [What agents can do on your machine](docs/what-agents-can-do.md): the full safety text.
- [Where Dudes keeps its data, and how to uninstall](docs/data-and-uninstall.md)
- [Build Dudes from source](docs/building-from-source.md)
- [Platforms and known limits](docs/platforms-and-limits.md)

## Contributing and security

[CONTRIBUTING.md](CONTRIBUTING.md) lists the checks to run before you push, and
[CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) sets the ground rules. Report security problems as
[SECURITY.md](SECURITY.md) describes, not in a public issue.

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the
work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
