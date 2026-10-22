# Contributing to Dudes

Thanks for helping. This document covers the mechanics; the design lives in the code and in the
doc comments on each crate.

## Ground rules

- **Licence.** Dudes is dual-licensed `MIT OR Apache-2.0`. There is no CLA and no DCO. Unless you
  explicitly state otherwise, any contribution you intentionally submit for inclusion is licensed as
  above, without any additional terms or conditions (Apache-2.0 §5).
- **Code of conduct.** See [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
- **Security.** Do not open public issues for security problems; see [SECURITY.md](SECURITY.md).
- **Questions and ideas** go to [Discussions](https://github.com/ArcfluxResearch/dudes/discussions);
  [SUPPORT.md](SUPPORT.md) says where each kind of request goes. Issues are for bugs you can
  reproduce and work that is ready to pick up.
- **Versions.** Do not change the version in a pull request. The maintainer bumps it once, in the
  pull request that prepares a release, so contributions never conflict on version files.

## Toolchain

`rust-toolchain.toml` pins the toolchain; `rustup` installs it on first use. The MSRV is
`rust-version` in `Cargo.toml` (policy: two stable releases behind the pinned toolchain at the time of
a release; a bump is a minor change and gets its own commit). CI is configured to check both.

Tools used by CI that you may want locally:

```sh
cargo install cargo-nextest cargo-deny --locked
```

## Before you push

CI runs every check on every pull request, so locally you only need the checks for what you
changed:

| You changed | Run locally |
|---|---|
| Only Markdown or other prose | `cargo xtask naming` |
| Only `floor/` | `npm run typecheck` and `npm test` in `floor/`, with the toolchain in [floor/README.md](floor/README.md) |
| Rust in one crate or binary | `cargo fmt --check`, then `cargo clippy --locked -p <package> --all-targets -- -D warnings` and `cargo nextest run --locked -p <package>` |
| Types in `crates/dudes-proto` | the Rust row, then `cargo xtask schemas` to regenerate `schemas/` and commit the result |
| Dependencies, or Rust across several crates | the full set below |

The full set, which is what CI runs:

```sh
cargo fmt --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo nextest run --workspace --locked --no-fail-fast
cargo deny --locked check
cargo xtask layering
cargo xtask schemas --check
cargo xtask naming
```

The [CI workflow](.github/workflows/ci.yml) runs on every pull request and every push to `main`:
`checks` (formatting, the `dudes-types` no-default-features build, crate layering, schemas and
naming), `clippy` on Linux, macOS and Windows, `test` on Linux and macOS, `msrv`, `deny`, `floor`
and `e2e`. Its weekly run adds the Windows tests.

When you change the Pi MCP extension or add a version to `PI_MCP_TESTED_VERSIONS`, run
`scripts/verify-pi-mcp.py` locally against the real Pi releases; CI runs only the extension's unit
tests.
See [floor/README.md](floor/README.md) for the Floor's toolchain and checks.

## Check a release before publishing

With the [Floor toolchain](floor/README.md) and its dependencies installed, run:

```sh
DUDES_SMOKE_REQUIRE_CHROME=1 cargo xtask smoke
```

The command builds the Floor and release binaries, serves the archive on `127.0.0.1`, and runs
`install.sh` into a temporary prefix. It runs the installed scripted quickstart with a temporary
HOME, stand-in engines, and an enrolled temporary git repository. If Chrome or Chromium is installed,
it opens the Floor through the CLI bootstrap link, hires through the wizard, and checks live activity
and `dudes agents`. It stops the daemon and removes its temporary files and processes.

Each stage prints `PASS` or `FAIL`. A failure exits non-zero. Release sign-off requires Chrome or
Chromium and fails if neither is installed. For a local install check, omit
`DUDES_SMOKE_REQUIRE_CHROME=1` to permit `SKIP browser (Chrome absent)`. The command uses no paid
engine turn and contacts only loopback addresses. Chrome DevTools uses inherited pipes.

To select a browser executable, set `DUDES_SMOKE_CHROME` to its absolute path. The path must name
an executable file. An invalid explicit value fails the check without falling back to discovery.
The build preserves `CARGO_BUILD_JOBS` and `CARGO_INCREMENTAL` when you set them.

macOS builds default `MACOSX_DEPLOYMENT_TARGET` to `12.0` through `.cargo/config.toml`. An explicit
lower value is refused by `dudes-host`'s build script; `12.0` and higher are accepted.

## Cut a release

1. Run `cargo xtask release-notes --draft --from <previous tag>`. It prints a draft changelog
   section from the pull requests merged since that tag.
2. Add the version's section to `CHANGELOG.md` as `## [<version>] - <date>`, with the compare and
   tag links at the bottom as Keep a Changelog does. Edit the draft, and write the Highlights
   by hand: two to four bullets that lead with what changes for the user.
3. Bump `version` in `[workspace.package]` in `Cargo.toml`, update `Cargo.lock` and the Floor
   release-manifest fixtures. The bump happens only in this release change, never in other pull
   requests.
4. Merge that release pull request to `main`.
5. Tag the merge commit `v<version>` and push the tag:

   ```sh
   git tag v<version>
   git push origin v<version>
   ```

Wait for the merge commit's CI on `main` to pass, or tag at once and let the release wait for it.
The [Release workflow](.github/workflows/release.yml) refuses a tag that is not on `main`, or whose
CI run on `main` failed, and waits up to 45 minutes for one still running. It then builds the Floor
once, and builds and smoke-tests an archive for each of the six targets `install.sh` supports. It
fails if the tag is not `v` plus the workspace version, and it fails early if `CHANGELOG.md` has no section for that
version, so a release never goes out without notes. It then writes one `SHA256SUMS`, builds the
release body from the changelog section and the pull requests since the previous tag, attests the
archives and `SHA256SUMS`, and creates the GitHub Release. Linux hosts install the static musl
archive, and the glibc archives are published for people who unpack them by hand. A tag with a
hyphen, such as `v1.0.0-rc.1`, is marked a pre-release, which `install.sh` does not pick as the
latest release.

Run the workflow by hand (Actions, Release, Run workflow) to build and smoke-test the archives
and preview the release notes without publishing. The body is uploaded as the `release-notes`
artifact, and it falls back to the Unreleased changelog section when the version has none yet.

## Layout and rules

- `crates/dudes-*` are libraries, `bins/` are the daemon (`duded`), the CLI (`dudes`) and the hook
  shim (`dudes-hook`); `xtask/` is repository automation; `floor/` is the browser view.
- Workspace lints are inherited (`[lints] workspace = true`). `unsafe_code` is **forbidden
  everywhere**. Three crates set `deny` instead: `dudes-pty`, `dudes-host` and `dudes-win`. Each
  crate that sets `deny` grants `unsafe` to **named modules** with `#[allow(unsafe_code)]`, never
  crate-wide, and pins the grant with a source scan that fails when it spreads
  (`crates/dudes-host/tests/unsafe_grant.rs` is the worked example). `forbid` is what forces that
  shape: unlike `deny` it cannot be re-levelled from source, so a crate inheriting the workspace
  table can hold no `unsafe` at all. Cargo also rejects a `[lints]` table that both sets
  `workspace = true` and adds entries, so each of those crates copies the whole workspace lints
  table with exactly three deltas: the `unsafe_code` line, plus `unsafe_op_in_unsafe_fn = "deny"`
  and `clippy::undocumented_unsafe_blocks = "deny"`. `cargo xtask lints` checks those copies and
  their source grants. Every `unsafe` block has a `// SAFETY:` comment.
- `clippy::pedantic` is on. Add to the allow list in `Cargo.toml` only with a reason.
- Crate layering is checked in CI (`cargo xtask layering`): `dudes-supervisor` does not depend on
  `dudes-pty` or `dudes-proxy`; `dudes-hook` links no async runtime, TLS stack or tracing subscriber.
  A new crate adds its row to `xtask/src/layering.rs`.
- `schemas/*.json` are generated from `dudes-proto` by `cargo xtask schemas`; never edit them by hand.
- Dependencies go through `[workspace.dependencies]`. New licences need a `deny.toml` change in the
  same PR, with the reason in the commit message.
- Tests name the invariant they protect in their doc comment, by its number in [Invariants](#invariants).

## Invariants

Code comments and tests cite these rules as "invariant N". Every change keeps all ten, and a pull
request that changes one says so in its description.

1. **One writer.** Persistent state exists only as records that the daemon appends to its journal.
   Everything else, such as the state database, the board, memory, the roster and the Floor, is a
   projection that can be rebuilt from the journal.
2. **Persist before spawn.** A turn is journaled (`TurnIssued`) and fsynced before its process is
   launched. Replay after a crash re-issues a turn with no recorded spawn once, under the same
   `TurnId` and task, except a browser-note issue the daemon durably abandoned, an owed reissue
   durably cancelled first, a turn whose agent is retired, halted, or otherwise held by a standing
   cause, or a ticket turn whose instruction review or comparison is still outstanding. Outside
   active closing time, a Managed goal turn found issued without a recorded spawn waits for explicit
   consent to repeat its exact task in fresh context. Its `TurnId`, task, answer list, and reservation
   remain unchanged. Another crash before spawn is recorded requires new consent. Reissue may launch
   a second process for one `TurnIssued`. It does not guarantee exactly-once process execution or
   external effects.
3. **Done is Observed.** `Done` requires a predicate result with `Provenance::Observed`: a command
   the daemon ran, a hash, a board state or a verifier verdict. An agent's report about its own work
   is evidence, never a verdict. An `Inferred` event may raise a breaker rung, but it is never the
   only cause of `Stop` or `Done`.
4. **Budget is admission.** A goal, agent or floor ceiling that is set is checked before a turn is
   issued. A turn over the ceiling is not issued, and no warning stands in for that. An unset ceiling
   is no cap.
5. **The loop is pure.** `dudes_policy::decide()` performs no I/O, reads no clock, and gives the
   same answer when replayed from the journal. Its gates run in this order: control, provenance,
   human, done, budget, hops, work.
6. **No REPL typing.** A message body is never typed as keystrokes into a live terminal or REPL.
   Attached mode may type one constant sentinel that carries no content, and Managed mode types
   nothing. A task is the fixed payload of one durably issued turn. It enters an owned provider
   process only through typed launch arguments, one write to its standard input followed by a
   close, or one typed task-start request while a provider protocol starts up. Protocol control
   messages cannot add another task to that process.
7. **No shell strings.** Commands are `Vec<OsString>` built from typed parts. The daemon does the
   `PATH` lookup, and nothing is interpolated into a shell.
8. **Identity is structural, and unproven identity is refused.** A mail's `from` is the directory
   that owns it. On the socket, identity is what a peer presents (the operator credential or a
   per-agent token), qualified by what the daemon can resolve about that peer on its own. It is
   never a field the sender fills in, and never the absence of a credential.
   `dudes_rpc::auth::classify` decides what each presentation resolves to, and each method's
   `Requirement` says who may call it. A transport that supplies no peer pid is refused before any
   method an agent can call. The peer credential admits a connection but cannot tell an operator
   from an agent, so the same user id is not a boundary.
9. **Layering.** `dudes-supervisor` depends on no data-plane crate, and `dudes-hook` links no async
   runtime, TLS stack or tracing subscriber. `cargo xtask layering` checks both, as
   [Layout and rules](#layout-and-rules) describes.
10. **Every child is owned.** Spawned processes live in a process group or job object and die with
    their agent or with the daemon. No listener binds outside loopback.

## Commits and pull requests

- [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/): `feat(journal): …`,
  `fix(policy): …`, `chore: …`. The changelog is generated from them.
- Keep PRs small and single-purpose. Squash-merge is the default.
- A PR that changes a wire format, a crate boundary or an invariant says so in its description.
- Signed commits are encouraged, not required.
