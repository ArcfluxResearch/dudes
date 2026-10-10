# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Maintainers prepare released sections from Conventional Commits before manually tagging a release.
`cargo xtask release-notes --draft --from <previous tag>` prints a draft. The release workflow
refuses a tag whose version has no section here, and builds the GitHub Release body from it.

## [Unreleased]

## [0.79.0] - 2026-10-10

The first public release. Dudes is an early preview, so commands and configuration keys can still
change between releases. [Platforms and known limits](docs/platforms-and-limits.md) lists what has
been tested and what is missing.

### Added

- **A daemon and a CLI that supervise coding agents.** The `duded` daemon owns all state and records
  it in an append-only journal. The `dudes` command talks to it over a local socket. You hire
  Claude Code, Codex or Pi agents, each with a name, a role and a working directory.
- **Goals with completion checks.** `dudes goal` gives an agent a piece of work and a command. The
  daemon runs that command itself after each turn, and the goal is Done only when the daemon's run
  passes. A hop cap and optional spending ceilings stop work that does not converge.
- **The Floor.** A 3D office in the browser shows each agent's status, goal and open questions. You can
  answer, message, pause, retire or attach to an agent from it, through the Floor actions you turn
  on.
- **Mail and teams.** Agents send each other messages with `dudes mail` and find colleagues with
  `dudes team`.
- **Worktrees.** `dudes hire --worktree` gives a Claude Code agent its own git worktree and branch in
  a repository you mark as trusted.
- **Memory.** Agents keep notes across turns with `dudes memory append` and `dudes memory read`,
  and a long history is condensed.
- **MCP servers.** You declare MCP servers once and choose which agents get them.
- **Closing time.** `dudes stop` gives each agent a bounded chance to save its work before the daemon
  stops. `dudes stop --hard` stops at once.
- **Guided setup.** `dudes quickstart` turns on engines, starts the daemon, opens the Floor and can
  hire a first agent. `install.sh --from` installs a local build into `~/.local` without `sudo`.

[Unreleased]: https://github.com/ArcfluxResearch/dudes/compare/v0.79.0...HEAD
[0.79.0]: https://github.com/ArcfluxResearch/dudes/releases/tag/v0.79.0
