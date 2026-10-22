# Build Dudes from source

Releases for macOS and Linux on x86_64 and arm64 install with the command in the
[README](../README.md#install). On Linux that installs a static build that runs on any
distribution. Build from a checkout to change Dudes, to run an unreleased commit,
or on Windows, which has no prebuilt release. You need git, a C compiler and linker (the Xcode
Command Line Tools on macOS, or `build-essential` or your distribution's equivalent on Linux), the
Rust toolchain (`rustup` installs the pinned version on first use) and Node 24.21.0 with npm
11.19.0 for the Floor. Build the Floor, assemble the binaries, and install them with
[install.sh](../install.sh):

```sh
git clone https://github.com/ArcfluxResearch/dudes.git
cd dudes
(cd floor && npm ci --ignore-scripts && npm run build)
cargo xtask dist
sh install.sh --from target/dist
```

[floor/README.md](../floor/README.md) has the Floor's checks. `cargo xtask dist` builds `duded`,
`dudes` and `dudes-hook` in release mode and writes them to `target/dist/dudes-<version>/` with
the built Floor beside them as `floor/`. It prints that directory. The daemon serves the Floor from
there, so no variable is needed when you run `duded` from that directory.

On macOS and Linux, the command also writes `target/dist/dudes-<version>-<target>.tar.gz`, named
by the host's target triple, or by the triple that `--target <triple>` names, and records its SHA-256 in `target/dist/SHA256SUMS`. These are local
build outputs. `install.sh --from target/dist` installs from them. It checks the archive against
`SHA256SUMS` before it installs anything, makes no network request, and cannot be combined with
`DUDES_INSTALL_BASE_URL`. By default it installs into `~/.local`; `--prefix <dir>` chooses another
place. Run `sh install.sh --help` for the options.

`cargo xtask dist --target <triple>` builds for one of the six targets the installer supports. It
runs the daemon it just built to check the Floor, so build only for a target this machine can run.
The release workflow builds each target on a matching runner.

Windows builds produce the directory but no archive, and a Windows installer is not implemented.

To build and test every crate:

```sh
cargo build --workspace
cargo nextest run --workspace
```

The toolchain is pinned in `rust-toolchain.toml`; `rustup` installs it on first use.
macOS builds require macOS 12 or later.
Windows builds require Windows 10 version 1809 or later.
See [CONTRIBUTING.md](../CONTRIBUTING.md) for contributor checks.
