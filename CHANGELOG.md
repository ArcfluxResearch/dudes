# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Maintainers prepare released sections from Conventional Commits before manually tagging a release.
`cargo xtask release-notes --draft --from <previous tag>` prints a draft. The release workflow
refuses a tag whose version has no section here, and builds the GitHub Release body from it.

## [Unreleased]

### Fixed

- Recognize native Linux ZFS and OverlayFS, including Docker filesystems, when sampling instruction files. Direct NFS, CIFS and SMB mounts remain refused. FUSE and unknown filesystem types also remain refused. OverlayFS may hide network-backed layers, whose locality and freshness Dudes does not establish.

## [0.87.0] - 2026-10-14

### Highlights

- **Review shared instruction changes.** Tickets require operator review when observed shared instructions change or their coverage is uncertain. The Floor and `dudes ticket show` display known changed paths, coverage gaps and observation details. Accept or Ask for changes applies to the exact reviewed state. (#57)
- **Upgrade and uninstall from the CLI.** `dudes upgrade` installs the latest release and restarts a running daemon; `--check` reports availability without installing. `dudes uninstall` previews removal and asks for confirmation. Both commands support macOS and Linux installer installations. Uninstall refuses to remove agent worktrees. (#55, #56)

### Changes

- Refuse inherited Claude context when a live peer shares the folder or owned-peer evidence is unresolved. Refusals explain when to use a separate worktree or wait for confirmed absence. (#57)
- When instruction review is pending, `dudes ticket accept` and `dudes ticket changes` require an explicit `--expected-seq` from `dudes ticket show`. (#57)
- Opt in to desktop notifications in Floor settings for tickets ready for review, tickets needing you and agent questions. Keep the Floor tab open; delivery depends on browser permission and the operating system.
- Open a ticket's recorded checkout with `dudes ticket open <card>`, using `VISUAL`, `EDITOR` or `--editor`. `--print` and `--json` inspect the directory without launching an editor. (#48)
- Document release checksum and provenance verification, plus Dudes and engine network/data boundaries. Add static regression checks for known outbound socket and DNS APIs. These checks do not sandbox child programs. (#50, #51)

### Fixes

- Refuse nonlocal Windows named-pipe overrides before attempting a control connection.
- Default repeated quickstart hiring to **No**, so pressing Enter does not hire another idle agent. (#47)

### Breaking

- Instruction observations and reviews add required journal records. Older readers refuse a journal containing these records; downgrading after they are written is unsupported. (#57)
- Rust ticket clients must include `instruction_review` in `TicketRow` and `CardTicket` constructors, and `review_watermark` in `TicketAssignParams` and `TicketAcceptParams`. (#57)
- Rust exhaustive matches must handle new variants in `Record`, `RecordKind`, `TicketReason`, `TicketRefusal`, `HoldReason`, supervisor `Effect` and `Seen`, and daemon `Answered` and `OnboardRefusal`. `OnboardRefusal` no longer implements `Copy`. (#57)

Instruction review compares bounded sampled endpoints. Windows and other unsupported platforms report coverage gaps. The comparison cannot identify an exclusive writer or prove that edits were not made and restored between samples.

## [0.86.0] - 2026-10-12

### Highlights

- **A board with room to work.** Board has its own working area, four counted workflow columns and a separate Needs you lane. All four columns fit at 1024 × 768, and Crew, office and terminal navigation keep their state when you leave Board. (#41, #44)
- **Review tickets in a side drawer.** Read the agent's summary, the daemon's check result and changed file names, then Accept or Ask for changes. New ticket opens a form with an optional agent; keyboard navigation and focus return keep the review flow usable. (#42)
- **See closing-time progress.** During `dudes stop`, the Floor shows acknowledged, settled, owed and running counts, plus time left. Disconnected views freeze their last observed values, and completion retains final counts. No Floor shutdown control or participant identities are added. (#45)

### Changes

- Give Board a full working area with a prominent New ticket action, counted workflow columns, empty-lane guidance and the newest 20 Done tickets. (#41)
- Add native ticket review and composer dialogs with plain-text content, state-appropriate actions and separated Stop/Remove confirmations. (#42)
- Add aggregate closing-time counts and a local countdown sampled from the daemon's existing deadline. Show early completion only when nothing remains owed or running and positive time remained; shutdown does not guarantee delivery of the final Complete frame to every subscriber. (#45)

### Fixes

- Keep ticket focus valid when a card changes or disappears, during nested Hire and after authority changes. Dismissed or replaced composers ignore late creation replies. (#42)
- Reduce workflow gaps in the existing tablet band so Done is fully visible at 1024 × 768, including the native vertical-scrollbar case. (#44)
- Publish closing-time updates in durable order and retain the original error when publication fails. (#45)

## [0.85.1] - 2026-10-12

### Highlights

- **Set up a project in one step.** Run `dudes workspace add` in a git repository to enroll it with its suggested completion check and sensible defaults. (#40)

### Changes

- Update TOML parsing and editing dependencies. (#37)
- `dudes workspace add` defaults to the canonical current directory, a unique folder id, the suggested check and its label, department `engineering`, five turns per goal and a 600-second check timeout. Explicit options override these defaults. (#40)
- Quickstart asks for a project folder outside a repository, explains when you skip it, and asks at most five questions including confirmation. Its first hire starts idle, and sign-in commands are printed. (#40)
- Without `--cwd`, `dudes hire` uses the only enrolled project or asks which one on a terminal. An agent hired without a project is named in a warning that explains it cannot take tickets. (#40)

### Fixes

- Start, Floor open, quickstart and ticket creation now explain when no project is set up and name `dudes workspace add` as the next step. The hint stays out of JSON output. The docs also explain how to bypass a shell alias or function named `dudes`. (#40)
- The Floor explains when tickets are off, when a project is missing, when no agent works in the selected project and when an agent hired without a project cannot take tickets. (#39)

## [0.85.0] - 2026-10-11

### Highlights

- **A ticket board.** Write a ticket, give it to an agent, and it starts at once. Each agent has a queue, and one ticket runs per project at a time. When the daemon's own run of the ticket's check passes, the ticket waits in Ready for you with the agent's summary, the changed files and the check result. You Accept it or Ask for changes. Tickets that can't go on wait in Needs you with a sentence that says what to do. Use it from the Floor's Board or with `dudes ticket`. (#19, #20, #21, #22, #23, #24, #25, #32, #33)
- **A quieter agent drawer.** The Output and Terminal tabs are gone: the full output opens from the agent's reply in Chat or from Overview, and Attach sits with the other actions. Open questions moved from the top bar to a "Waiting on you" section on Overview, which also lists tickets that need you. (#15, #16, #17)
- **Upgrades tell you to restart.** After an install, and in `dudes ping` and `dudes floor open`, Dudes says when the running daemon is still an older version and how to replace it. (#18)

### Changes

- New `dudes ticket` commands: `add`, `edit`, `give`, `show`, `list`, `accept`, `changes`, `stop` and `remove`. `--agent` takes an agent's name or id. (#21, #24)
- Giving a ticket starts the agent at once when it and the project are free, and queues it otherwise. A ticket sent back for changes goes ahead of new ones. Stop ends the agent's goal and puts the ticket back in To do; the agent's current step finishes first. (#22, #23)
- A ticket reaches Ready for you only after the daemon's own check passes following the agent's work. Only you move it to Done. Agents can't move ticket cards themselves. (#19, #20, #22)
- The Floor's Board shows To do, Working, Ready for you and Done, plus Needs you, with a ticket form, a panel for each ticket and a banner when one is ready or needs you. It is behind the new `floor.board` switch, which needs `floor.control` and `floor.goal`. `dudes quickstart`'s one Floor question now includes it and says that agents then run and spend under your account. To turn it on for an existing setup, run `dudes config set floor.board on` and restart the daemon. (#25, #32, #33)
- Once you create a ticket, the journal holds records that older versions of Dudes refuse to open, so downgrading after that is not possible. (#19)
- Releases are published only from commits on `main` whose CI passed. (#30)

### Fixes

- After an upgrade, a daemon left running on the old version is now named, instead of silently serving the old Floor. (#18)

## [0.84.0] - 2026-10-11

### Highlights

- **Quickstart asks less.** It turns on the engines it finds on your `PATH` and asks at most five questions: one consent question for Floor actions, the repository's completion check, and a first hire. It asks about a daily spending cap only when an API key is set. (#4)
- **The Floor link is printed, never opened.** `dudes floor open` and quickstart print a one-time link to paste into any browser on this computer. A link that was already used or has expired says so and tells you how to get a new one. (#5)
- **`dudes start` makes upgrading two commands.** It starts the daemon without any setup questions and warns when the running daemon is a different version from `dudes`. After installing a new release, run `dudes stop`, then `dudes start`. (#8)
- **A quieter Floor.** The spend totals card and the Traces tab are gone until there is a place for analytics, and the selected agent shows one status line and one row of actions. (#2, #3)

### Changes

- Quickstart detects engines instead of asking, asks one question for all Floor actions, and asks about a spending cap only when an API key is set. Pressing Enter at the Floor question turns Floor actions on; answer No to keep the Floor view-only. When no engine is on your `PATH`, the first-hire question defaults to No. (#4)
- `dudes floor open` and quickstart print the Floor link and never open a browser. `--no-browser` is still accepted and does nothing. A reused, expired or missing sign-in link gets its own message, and the docs explain how to reach the Floor over SSH. (#5)
- New `dudes start` command. It starts the daemon when it is not running, prints its version, socket and journal position, and names a version mismatch. Restart advice in the CLI and the docs now says `dudes stop`, then `dudes start`. (#8)
- The Floor no longer shows the spend totals card or the Traces tab. The daemon still records the same data. (#2)
- The selected agent's header in the Floor shows one status line and a compact row of actions. (#3)
- Releases carry structured notes built from this changelog, and each archive and `SHA256SUMS` carry a build provenance attestation you can check with `gh attestation verify`. (#7)

### Fixes

- The Floor's Git view no longer reports a large repository as unreadable at random on Linux. Git ran out of room to start its helper threads under the daemon's memory limit; the daemon's git reads now run without them. (#6)

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

[Unreleased]: https://github.com/ArcfluxResearch/dudes/compare/v0.87.0...HEAD
[0.87.0]: https://github.com/ArcfluxResearch/dudes/compare/v0.86.0...v0.87.0
[0.86.0]: https://github.com/ArcfluxResearch/dudes/compare/v0.85.1...v0.86.0
[0.85.1]: https://github.com/ArcfluxResearch/dudes/compare/v0.85.0...v0.85.1
[0.85.0]: https://github.com/ArcfluxResearch/dudes/compare/v0.84.0...v0.85.0
[0.84.0]: https://github.com/ArcfluxResearch/dudes/compare/v0.79.0...v0.84.0
[0.79.0]: https://github.com/ArcfluxResearch/dudes/releases/tag/v0.79.0
