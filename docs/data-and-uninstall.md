# Where Dudes keeps its data, and how to uninstall

## Where Dudes keeps its data

Dudes keeps its data in two folders under your home directory. On Windows, when `HOME` is not
set, both sit under `%USERPROFILE%` instead.

- The state directory is `~/.local/share/dudes`, or `DUDES_STATE_DIR` when set. Among other files,
  it holds the journal (`journal/`) and the projection built from it (`state.redb`), a folder for
  each agent (`agents/`), the Codex streaming profile (`providers/codex/`) and every agent worktree
  (`worktrees/`). An agent's uncommitted work in a worktree exists only there. When `install.sh`
  runs with its default prefix, it also puts the Floor (`floor/`) and the licence notices here.
- The runtime directory is `~/.local/state/dudes`, or `DUDES_RUNTIME_DIR` when set. Among other
  files, it holds the control socket (`duded.sock`), the operator credential (`operator.cred`), the
  daemon's log (`duded.log`), a receipt for each stop and the Floor's one-time sign-in files.

Dudes also leaves files in these places:

- The operator file, `config.toml`, sits at your platform's path from
  [Configuration](configuration.md#the-operator-file), or at the absolute path in `DUDES_CONFIG` when set. The
  daemon and `dudes` both read the variable. A relative or empty value is refused, so a typo
  cannot make Dudes read a different file.
- In each repository where you hire with `--worktree`, Dudes registers a git worktree for the
  agent and creates a branch named `dudes/<id>`, where `<id>` is 12 hexadecimal characters. Dudes
  never merges or pushes these, and it never deletes a tree that an agent ran in.
- `install.sh` puts `duded`, `dudes` and `dudes-hook` in `~/.local/bin`, or in `<dir>/bin` when
  you pass `--prefix <dir>`. A source build also leaves them in `target/dist/` in your checkout, where
  `cargo xtask dist` wrote them.
- Claude Code and Codex keep their own session history in their own folders, for example
  `~/.claude/projects/` and `~/.codex/sessions/`. Uninstalling Dudes does not delete it. Pi's
  sessions and the Codex streaming profile are in the state directory.
- The Floor keeps view preferences, such as a pinned office, activity filters and the terminal's
  font size, in your browser's storage for its `127.0.0.1` address.

If you ran Codex agents with an earlier version of Dudes, delete old Codex shell snapshots. Codex
writes these scripts to `shell_snapshots/` in its home, and each one exports the environment a
turn started with, including the agent's Dudes credential. Dudes now turns them off. Before that,
a turn that was stopped, for example by `dudes quickstart --restart`, could leave one behind.
Delete each file there that contains a `DUDES_TOKEN=` line, in your Codex home (`~/.codex`, or
`CODEX_HOME` when set) and in the Codex streaming profile. On macOS and Linux, with the default
folders:

```sh
find ~/.codex/shell_snapshots ~/.local/share/dudes/providers/codex/shell_snapshots \
  -type f -exec grep -q 'DUDES_TOKEN=' {} \; -print -delete
```

## Uninstall Dudes

Run `dudes uninstall` on macOS or Linux. It prints every path it will delete with its size, asks
for confirmation on a terminal, and deletes in a safe order: the daemon is stopped, then the
runtime directory, the state directory, the operator file, and last the three programs, so you can
run it again if something fails part way.

```sh
dudes uninstall
```

- **Agent worktrees stop it.** While any tree exists under `worktrees/` in the state directory,
  `dudes uninstall` deletes nothing. It prints the repository each tree belongs to and the git
  commands from steps 3 and 4 below, so you can review and remove each tree first.
- **A running daemon is stopped first**, the way `dudes stop` does it. If an agent is running a
  turn, waiting for your answer or attached to your own session, it refuses and names each one.
  `--now` stops the daemon with closing time anyway, which gives every agent up to 6 minutes to
  save its work. To end turns at once, run `dudes stop --hard` first.
- **Without a terminal it needs `--yes`.** `--dry-run` prints the plan and deletes nothing.
  Answering no at the prompt changes nothing and exits 0.
- **Guards.** It touches a data directory only at its default path, or at a `DUDES_STATE_DIR` or
  `DUDES_RUNTIME_DIR` whose content only Dudes writes. In a folder that a setting chose, including
  the operator file's folder, it deletes only the files Dudes writes and removes the folder only if
  that leaves it empty; anything else stays, and it says what. It never deletes the filesystem
  root, your home directory, a parent of it, or a symbolic link. With `DUDES_CONFIG` set it deletes
  only that file, and only if it reads as an operator file.
- **Programs.** It deletes `duded`, `dudes` and `dudes-hook` only when this copy was put there by
  `install.sh`. For a source build, `cargo install` or a link, it removes the data, leaves the
  programs and prints the commands to remove them by hand.
- **Exit codes.** A deliberate refusal exits 3, and a failure part way exits 4 and lists what
  remains. Run the command again once the cause is fixed.
- **It cannot clean up everything.** It prints, and does not do, the cleanup that lies outside
  Dudes: the engines' own session history, Codex shell snapshots that hold a `DUDES_TOKEN=` line,
  and your browser's site data for the Floor.

`dudes uninstall` is not supported on Windows yet; follow the steps below there.

### Uninstall by hand

If `dudes uninstall` is not available, or you want to do it yourself, remember that the state
directory holds agent worktrees. If you delete it first, you lose any uncommitted agent work, and
your repositories keep stale worktree entries. Remove Dudes in this order:

1. Stop the daemon. To see which agent owns each worktree, run `dudes worktree list` first.
   `dudes stop --hard` ends running turns at once instead of waiting for closing time.

   ```sh
   dudes stop
   ```

2. List the agent worktrees. Each one is a folder under `worktrees/` in the state directory. For
   each tree, `git worktree list` prints the repository it belongs to on its first line, and shows
   the tree's branch, `dudes/<id>`, in brackets.

   ```sh
   ls ~/.local/share/dudes/worktrees
   git -C ~/.local/share/dudes/worktrees/<tree> worktree list
   ```

3. In each tree, look for uncommitted work. In the output, `??` marks an untracked file, `M` a
   modified one, and `!!` an ignored file that step 4 deletes. Commit what you want to keep, or
   copy it out.

   ```sh
   git -C ~/.local/share/dudes/worktrees/<tree> status --short --ignored
   ```

4. Remove each tree, then its branch, from the repository that step 2 printed first.
   `git worktree remove` refuses a tree with modified or untracked files unless you add
   `--force`, and it deletes the tree's ignored files. `git branch -d` refuses a branch whose
   commits are not merged, and `git branch -D` deletes it anyway.

   ```sh
   git -C <repository> worktree remove ~/.local/share/dudes/worktrees/<tree>
   git -C <repository> branch -d dudes/<id>
   ```

5. Delete the two data folders. If you set `DUDES_STATE_DIR` or `DUDES_RUNTIME_DIR`, delete those
   folders instead.

   ```sh
   rm -rf ~/.local/share/dudes ~/.local/state/dudes
   ```

   Then delete the operator file's folder. On macOS:

   ```sh
   rm -rf ~/"Library/Application Support/dudes"
   ```

   On Linux:

   ```sh
   rm -rf "${XDG_CONFIG_HOME:-$HOME/.config}/dudes"
   ```

   On Windows, delete `.local\share\dudes` and `.local\state\dudes` under `HOME`, or under
   `%USERPROFILE%` when `HOME` is not set. Then delete `%APPDATA%\dudes`.

6. Delete the programs. If `install.sh` installed them, delete `duded`, `dudes` and `dudes-hook`
   from `~/.local/bin`. With `--prefix <dir>`, delete them from `<dir>/bin` and also delete
   `<dir>/share/dudes`. After a source build, also delete `target/dist/` in your checkout, and remove any `PATH`
   entry you added for it.

7. Optionally, clear your browser's site data for the Floor's `127.0.0.1` address to remove its
   view preferences.

If you deleted the state directory before step 4, clear the stale entries in each repository where
you hired an agent with `--worktree`. The `dudes/` branches stay until you delete them.

```sh
git -C <repository> worktree prune
```

