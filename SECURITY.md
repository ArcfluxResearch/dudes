# Security policy

Dudes spawns coding-agent CLIs, listens on a local socket, and mints per-agent tokens for hook
callbacks. Bugs on those edges are security bugs. Please report them privately.

## Supported versions

Dudes is pre-1.0. Only the latest release is supported. Security fixes ship in a new release, so
upgrade to it as the README describes.

## Reporting a vulnerability

Report vulnerabilities privately through GitHub:
[Report a vulnerability](https://github.com/ArcfluxResearch/dudes/security/advisories/new) opens a
private report that only the maintainer can see. If you cannot use GitHub, email
[alex.nthnz@gmail.com](mailto:alex.nthnz@gmail.com). Alex Nguyen monitors this address.
Do not open a public issue or pull request for a security problem.

Please include: the affected version or commit, the platform, a reproduction (a journal excerpt,
a config, or a command sequence), and the impact as you understand it.

What to expect:

- acknowledgement within 3 business days;
- an initial assessment within 10 business days;
- a fix or mitigation, a CVE if warranted, and credit in the fix's advisory or commit message
  (unless you prefer not to be named). We coordinate the disclosure date with you; we aim for 90
  days at most.

## In scope

- the daemon's local RPC socket and its peer-credential authentication;
- hook tokens: minting, transport, validation;
- command construction and process spawning (argument handling, environment, working directory);
- child process ownership (process groups / job objects) and cleanup;
- the journal and its projections (integrity, replay, file permissions);
- the mailbox (attribution of `from`, delivery, quarantine);
- installers and distribution artefacts.

## Out of scope

- vulnerabilities in the coding-agent CLIs Dudes supervises (report those to their vendors);
- attacks that require the attacker to already run code as the same user on the same machine,
  unless they cross a boundary Dudes claims to enforce;
- an agent or a completion check using the access that the README's
  [What agents can do on your machine](README.md#what-agents-can-do-on-your-machine) describes;
- denial of service by exhausting a budget you configured.

## Supply chain

`cargo xtask dist` builds binaries with `cargo build --release --locked`. On macOS and Linux,
it writes an archive and records its SHA-256 in `SHA256SUMS`. The install script checks the
downloaded archive against that file.

The [release workflow](https://github.com/ArcfluxResearch/dudes/blob/main/.github/workflows/release.yml)
adds [GitHub build provenance attestations](https://docs.github.com/en/actions/how-tos/secure-your-work/use-artifact-attestations/use-artifact-attestations)
for the archives and `SHA256SUMS`. Before publication, the workflow requires a tag commit reachable
from `main` and a successful `main` push CI run for that exact commit.

### Verify a release

1. Download the archive for your platform and `SHA256SUMS` from the same
   [GitHub release](https://github.com/ArcfluxResearch/dudes/releases).
2. In the download directory, check the archive against `SHA256SUMS`.

   On Linux, run:

   ```sh
   sha256sum --ignore-missing --check SHA256SUMS
   ```

   On macOS, run:

   ```sh
   shasum -a 256 --ignore-missing --check SHA256SUMS
   ```

   Confirm that the output names your downloaded archive and reports `OK`. These commands ignore
   missing archives for other platforms. The checksum check detects a mismatch with the listed
   hash. It does not authenticate the publisher.
3. With the [GitHub CLI](https://cli.github.com/) installed, verify the archive's attestation.
   Replace `dudes-VERSION-TARGET.tar.gz` with your downloaded archive's filename:

   ```sh
   gh attestation verify dudes-VERSION-TARGET.tar.gz --repo ArcfluxResearch/dudes
   ```

   Verify the checksum file's attestation too:

   ```sh
   gh attestation verify SHA256SUMS --repo ArcfluxResearch/dudes
   ```

   See the [GitHub CLI attestation verification reference](https://cli.github.com/manual/gh_attestation_verify)
   for the repository identity checks and verification options.

Extract or install the archive only after the checksum check and attestation verification succeed.

### Dependency checks

CI is configured to run `cargo deny --locked check` for RustSec advisories, licences, bans and
sources on pull requests and weekly. Dependabot is configured to open update pull requests for
Cargo and GitHub Actions dependencies. These configurations do not prove that checks ran or passed
for a particular commit.
