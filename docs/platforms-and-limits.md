# Platforms and known limits

Dudes is pre-alpha. It has managed provider adapters for Claude Code, Codex and Pi. Commands and
configuration keys can still change between releases.

## Platforms

The full first run, from `dudes quickstart` to a goal the daemon marks Done, has been rehearsed
on macOS on Apple silicon (arm64) and on Linux on arm64 (Ubuntu 24.04, with a stand-in for the
agent CLI). Builds for Linux on x86_64, Windows and Intel Macs have not had that rehearsal, so
expect gaps there. Releases carry archives for macOS and Linux on x86_64 and arm64,
and `install.sh` installs them. On Linux it installs a static build that runs on any distribution;
each release also carries a glibc build, which needs glibc 2.39 or newer and is not installed by
the script. Windows builds produce the binaries but no archive, and a
Windows installer is not implemented (see [Build Dudes from source](building-from-source.md)).

## Mail between agents

Mail between agents (`dudes mail`) has been rehearsed only with Claude Code agents, and it is not
available on Windows, so agents there cannot message each other or hand off work by mail.

On macOS and Linux, the daemon puts a directory that holds only its own `dudes` first on each
managed agent's `PATH`, so an older `dudes` further along cannot answer an agent's `dudes mail`
commands. For Claude Code agents it also removes a shell function or alias named `dudes` before each
command; a managed Claude Code agent therefore does not run your own `CLAUDE_ENV_FILE`. On Codex and
Pi, a shell function or alias named `dudes` still breaks agent mail, so remove it from the shell
those agents run.

## Pausing and the hook shim

When an agent is paused, its next tool call and its next prompt are refused through the hook shim
(`dudes-hook`). If the daemon accepts the shim's connection and then ends it without answering, for
example because it is at its connection limit, the shim blocks that step and asks you to retry. If
the shim cannot connect at all, because the daemon is stopped, it lets the step through so that a
stopped daemon never locks the agent. Other hook events always let the agent continue when delivery
fails.

On Windows the shim fails open in the connection-limit case: a busy or refused pipe fails at
connect, and a paused agent's steps are not blocked then.

## Separate worktrees

A separate git worktree for each agent (`dudes hire --worktree`) needs Claude Code and a repository
you mark as trusted with `--trusted`.
