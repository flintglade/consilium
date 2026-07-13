# Contributing to Consilium

Consilium welcomes public bug reports, documentation improvements, tests, new
provider adapters, accessibility work, and focused feature contributions.
All functionality must remain available in the public Apache-2.0 codebase.

By intentionally submitting a contribution, you agree that it is licensed
under the project's [Apache License 2.0](LICENSE), as described in section 5
of that license. Do not submit code, model weights, credentials, or other
material that you do not have permission to distribute.

## Project principles

- Keep Consilium fully usable with a local OpenAI-compatible endpoint.
- Do not add telemetry, advertisements, account requirements, license checks,
  proprietary Consilium backend dependencies, or paid feature gates.
- It is fine to add an optional connector to a third-party service, including
  a paid service, when it is clearly labeled and the local application remains
  complete without it.
- Keep provider authentication isolated. Official CLI credentials belong to
  the CLI; direct API keys belong in the user's environment and never in
  source, logs, fixtures, transcripts, or screenshots.
- Describe open-weight models accurately. Their model licenses are separate
  from Consilium's Apache-2.0 source license.
- Prefer small, direct implementations with clear error and cancellation
  behavior over speculative abstraction.

## Prerequisites

The checked-in `rust-toolchain.toml` selects the supported Rust release and
`Cargo.lock` fixes the Rust dependency graph. Node.js is needed only for the
dependency-free frontend test scripts; CI currently uses Node 24.

On Debian or Ubuntu:

```bash
sudo apt-get update
sudo apt-get install -y \
  build-essential libgtk-3-dev libwebkit2gtk-4.1-dev \
  libayatana-appindicator3-dev librsvg2-dev patchelf
```

Never put a real secret in a tracked file. Start local configuration with:

```bash
cp .env.example .env
chmod 600 .env
```

`.env` and `target/` are ignored. Keep `Cargo.lock`, `.env.example`, tests,
and public documentation tracked.

## Clean, reproducible build

From a fresh clone at the commit being tested:

```bash
rustc --version
cargo --version
node --version
cargo fetch --locked
cargo build --locked --release --workspace
```

The release executables are:

```text
target/release/grok-chat
target/release/grok-chat-desktop
```

`--locked` makes Cargo fail instead of silently changing the dependency graph.
For the closest repeatability, use the same OS image, compiler, linker, and
absolute source path. The repository provides a reproducible procedure and
locked inputs, but does not claim byte-for-byte identical native binaries
across different operating systems or linkers.

## Required validation

Run the complete local gate before submitting:

```bash
cargo fmt --all -- --check
cargo check --locked --workspace
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
for test in desktop/ui/*.test.js; do node "$test"; done
packaging/generate-third-party-notices.sh --check
packaging/check-distribution.sh
cargo build --locked --release --workspace
```

`make test` is the convenient Rust-plus-frontend test entry point. The
individual commands above are the authoritative contribution checklist and
match CI.

For desktop changes, launch `make run-desktop` and exercise the default route,
request cancellation, provider/model changes, session reopen, and the failure
state you changed. A process printing that it is listening is not a functional
test. For terminal changes, run `make run` in a real terminal and verify input,
streaming, cancellation, resize, and exit behavior.

Tests must not depend on paid API calls or a contributor's credentials. Use
parser fixtures, a fake CLI executable, or a local mock HTTP server. Optional
credentialed smoke tests can supplement, but never replace, deterministic
public tests.

## Provider adapter checklist

A new adapter should include:

1. A provider-specific command builder or HTTP client isolated in the core.
2. Streaming normalization into the shared event types.
3. Cancellation, non-zero exit/status handling, malformed-output handling,
   and a useful setup error.
4. Immutable model, effort, agent, and provider settings for each request.
5. Parser and command tests that run without accounts or network access.
6. Honest catalog metadata, authentication boundaries, billing language, and
   documentation links.
7. No fallback after answer text has begun.

Agent mode must only be advertised for an adapter that actually provides the
documented tool runtime and permission posture.

## Session persistence changes

Treat transcript integrity as data-loss-sensitive. Preserve atomic replacement,
same-filesystem temporary files, file and directory synchronization,
last-known-good backup behavior, and refusal to overwrite corrupt JSON. Add a
temporary-directory test for both success and failure paths. Never use a live
user data directory in a test.

## Pull requests

Keep each change reviewable and explain:

- the user-visible outcome;
- important design or privacy decisions;
- exactly what was tested;
- provider accounts or hardware needed for any optional manual validation;
- documentation or migration impact.

Review your own diff before publishing. Exclude `.env`, transcripts, provider
tokens, build output, crash dumps, screenshots containing private prompts, and
editor metadata.

## Release checklist

Maintainers can produce a public source-and-binary release with this
repeatable process:

1. Start from a clean tree and a reviewed commit.
2. Ensure the versions in the three Cargo package manifests and
   `desktop/src-tauri/tauri.conf.json` agree. Regenerate and review
   `THIRD_PARTY_NOTICES.txt` whenever `Cargo.lock` changes. Review any required
   `packaging/license-fallbacks.json` or
   `packaging/attribution-supplements.json` update against immutable upstream
   or exact locked-package material and its recorded SHA-256. Generation stops
   rather than filling in missing metadata or generic copyright placeholders.
   Preserve the release-text rules in `.gitattributes`; they prevent Windows
   checkout conversion from changing reviewed payload bytes.
3. Run `packaging/check-distribution.sh --release vVERSION` and every command
   in **Required validation**. Confirm both the Linux and Windows CI jobs pass
   on the exact commit before creating its tag.
4. Push an annotated `vVERSION` tag from that exact commit. The release
   workflow builds AppImage, deb, and RPM packages on Linux plus NSIS, MSI,
   and portable ZIP packages on Windows, then creates a draft release.
5. Let `packaging/verify-release-assets.sh` inspect the six downloaded
   artifacts. It checks package versions and publisher metadata, project
   licenses and dependency notices, MSI properties, exact portable contents,
   and x86-64 Windows application payloads before the workflow publishes
   `SHA256SUMS`.
6. Install the AppImage, deb, RPM, NSIS, MSI, and portable ZIP on clean target
   machines or VMs. Record Windows signing status and any optional provider
   smoke tests in the release notes.
7. Publish the draft only after those checks pass. Publish the source commit,
   tag, `Cargo.lock`, `THIRD_PARTY_NOTICES.txt`, release notes, application
   packages, and checksums together.

For an additional local checksum record of source-built binaries:

```bash
sha256sum target/release/grok-chat \
  target/release/grok-chat-desktop > SHA256SUMS.local
```

The base Tauri config keeps bundling off for ordinary workspace builds.
Platform overlays enable AppImage/deb/RPM on Linux and NSIS/MSI on Windows;
use those overlays through the tag workflow rather than treating a raw binary
as an installer.

## Community conduct

Be respectful, specific, and constructive. Harassment, discrimination,
credential sharing, malicious contributions, and disclosure of another
person's private prompts or data are not acceptable. Report sensitive private
issues through the repository host's private reporting channel rather than a
public issue when that channel is available.
