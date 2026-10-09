# Consilium distribution plan

Consilium uses the Flintglade name and dragon mark for distribution. The
stable application ID is `com.flintglade.consilium`; changing it later would
break desktop identity and require a new Flathub submission.

Consilium itself is free and open source under Apache-2.0. Release packages
link to [Flintglade](https://flintglade.com/), source and issues live in the
[canonical repository](https://github.com/flintglade/consilium), and optional
project support is available through
[Patreon](https://www.patreon.com/c/zach457). Third-party providers may charge
for their own services, but payment never unlocks a Consilium feature.

## Current release channels

The tag workflow builds native Tauri bundles on their target operating
systems:

| Platform | Downloadable artifacts | Build host |
| --- | --- | --- |
| Linux x86_64 | AppImage, Debian package, and RPM | Ubuntu 22.04 |
| Windows x86_64 | NSIS setup executable and MSI | current GitHub Windows runner |
| Windows x86_64 portable | ZIP containing `Consilium.exe`, project license, dependency notices, and a usage README | current GitHub Windows runner |

Every Linux package includes the repository `LICENSE`: the deb installs it as
`/usr/share/doc/consilium/copyright`, while the RPM and AppImage install it as
`/usr/share/licenses/consilium/LICENSE`. Each package also includes the exact
generated `THIRD_PARTY_NOTICES.txt`; the local Flatpak installs it beside its
project license. The portable ZIP contains both `LICENSE.txt` and
`THIRD_PARTY_NOTICES.txt`. Linux package metadata records
`Flintglade <support@flintglade.com>` as the deb Maintainer and
`https://flintglade.com/` as the deb Homepage and RPM URL.

The notice inventory comes from `cargo metadata --locked` and records each
third-party package's name, version, declared license expression, precise
crate version page, direct source-archive URL, and Cargo source without
normalizing or guessing. It uses `cargo tree --locked -p grok-chat-desktop`
for the two native release targets and embeds every license, copyright,
attribution, and notice file found in those exact locked crate archives. This
keeps the material section aligned to the desktop executable actually shipped,
while the complete inventory still covers all workspace dependencies. The
source-package links provide the locked source corresponding to the binary.
Regenerate and review the notice after any lockfile change:

```bash
packaging/generate-third-party-notices.sh
packaging/generate-third-party-notices.sh --check
```

Twelve release crates currently omit their license files from their published
crate archive. `packaging/license-fallbacks.json` maps them to exact
commit-pinned upstream files or exact files from another locked crate, records
reviewed SHA-256 hashes, and must cover that missing set exactly. Generation
stops if metadata, source files, hashes, UTF-8 text, or fallback coverage drift.
Nineteen MIT or legacy MIT/Apache materials contain a generic permission text
without a concrete holder line. `packaging/attribution-supplements.json`
covers that set exactly with commit-pinned upstream COPYRIGHT files or exact
authorship/source notices from the locked package. This avoids inventing a
holder or treating a generic template as attribution. Repository attributes
also disable checkout newline conversion for the generated notice and both
reviewed material sets, while fixing the project license to LF, so Linux
inspection compares the same reviewed bytes that Windows packaging received.

The Windows installers use Tauri's resource mapping to install the notice file
with the application. A Tauri installer accepts only one `licenseFile` for its
license-dialog page, so the project `LICENSE` remains that page and dependency
notices are delivered as a separate installed resource instead of being
merged into the dialog.

Push an annotated `vMAJOR.MINOR.PATCH` tag only after all package versions and
`tauri.conf.json` agree. The workflow validates the tag, creates a **draft**
release, attaches all six application artifact types, and uploads
`SHA256SUMS`. On a workflow rerun, any previously published checksum file is
removed from the download and explicitly excluded from its own input set. The
owner must install each artifact on a clean machine or VM before publishing
the draft. Tauri's updater is intentionally absent until an updater endpoint
and signing key are owned, backed up, and documented.

The final workflow downloads all draft assets and runs:

```bash
packaging/verify-release-assets.sh release-assets
```

That check requires one artifact of each expected type; compares every bundled
project license and dependency notice to the repository copies; inspects deb,
RPM, and MSI metadata; requires the portable ZIP's exact four-file layout; and
checks the portable, MSI, and NSIS application payloads as x86-64 PE32+
executables before generating `SHA256SUMS`. It deliberately inspects the
application embedded in NSIS rather than inferring product architecture from
the NSIS launcher stub, whose architecture can differ from its payload.

Windows bundles are unsigned unless the release owner configures an owned code
signing certificate outside the repository. Unsigned artifacts may trigger
SmartScreen. Signing secrets, certificate files, passwords, and timestamping
credentials must live in protected CI secrets, never source. Publishing an
unsigned installer is an explicit owner decision that must be stated in the
release notes.

The portable ZIP is the raw desktop executable produced at the Cargo workspace
path `target/release/grok-chat-desktop.exe`, renamed to `Consilium.exe` and
packaged with the README, project license, and dependency notices. It does not
install shortcuts or an uninstaller and it does not carry Tauri's WebView2
bootstrapper; the machine must already have a current Microsoft WebView2
Runtime. "Portable" applies to the executable, not its state: session data
defaults to
`%LOCALAPPDATA%\Flintglade\Consilium\sessions.json`, not inside the ZIP.
`GROK_CHAT_DATA_DIR` remains available when a custom private location is
preferred. The raw-executable path is an extra convenience channel and does
not replace clean-machine tests of the supported installers.

Linux AppImages should be built on the oldest supported base with WebKitGTK
4.1; the workflow uses Ubuntu 22.04 for that compatibility boundary. The
AppImage, deb, and RPM are direct-download formats, not a claim of app-store
publication. The AppImage embeds validated AppStream metadata under
`usr/share/metainfo` so catalogs can read the project description, release,
homepage, source, and optional-support links directly from the artifact.

## AppImageHub

The [Consilium catalog submission](https://github.com/AppImage/appimage.github.io/pull/3790) was merged on September 26, 2026. AppImageHub is a community catalog, not a file host. Consilium's AppImage must
remain directly downloadable from a public GitHub release; it must not be
wrapped in a ZIP or require authentication. Once a stable release passes the
artifact check and runs on the catalog's supported compatibility baseline, the
owner can add a one-line data file linking to
`https://github.com/flintglade/consilium` and open a pull request to the
AppImageHub catalog repository.

The published release asset is named
`consilium-<version>-x86_64.AppImage`, following AppImage's portable filename
convention even though Tauri's intermediate bundle uses Debian's `amd64`
architecture label.

The catalog runs its own automated checks and may apply additional human
review. A passing Consilium release makes submission possible, but it does not
guarantee listing. The release owner should submit only after checking the
extracted AppImage metadata, icon, desktop entry, first-launch state, and
download URL. AppImageHub continues to link to the GitHub-hosted file after a
listing is accepted; it does not take over release hosting.

## Flatpak and Flathub

`packaging/flatpak/com.flintglade.consilium.local.yml` is a real local Flatpak
recipe. It builds from the working tree with an offline Cargo source manifest,
installs the Flintglade icons, desktop entry, AppStream metadata, and Apache
license, and grants only display, IPC, and network access. No home-directory
permission is present. The Flatpak can use local OpenAI-compatible endpoints
and direct APIs; host-installed provider CLIs are outside the sandbox.

Local validation:

```bash
packaging/check-distribution.sh
flatpak remote-add --user --if-not-exists flathub \
  https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak-builder --force-clean --user --install-deps-from=flathub \
  --repo=flatpak-repo flatpak-build \
  packaging/flatpak/com.flintglade.consilium.local.yml
flatpak build-bundle flatpak-repo Consilium.flatpak \
  com.flintglade.consilium \
  --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo
```

The local manifest is deliberately **not** a Flathub submission manifest.
Before any submission, a human owner must complete every gate below:

1. Establish the owner-confirmed canonical source repository at
   `https://github.com/flintglade/consilium`, create a stable tag, and make the
   repository and tag reachable without authentication.
2. Replace the local `dir` source with that immutable tag archive plus its
   verified SHA-256 hash. Regenerate `cargo-sources.json` from the tagged
   `Cargo.lock` using Flathub's `flatpak-builder-tools` Cargo generator. Flathub
   builds have no network access and must build entirely from declared source.
3. Confirm that GNOME runtime `50` is still the newest supported runtime at
   submission time; update it if necessary and rebuild both x86_64 and aarch64.
4. Verify the app through the domain derived from `com.flintglade.consilium`.
   The owner has identified `flintglade.com` as their domain; Flathub will
   provide a unique token that the owner must publish at its required HTTPS
   well-known path. Do not rename the ID after release casually.
5. Capture privacy-safe Linux screenshots under the rules in
   `packaging/screenshots/README.md`, publish them at immutable HTTPS URLs from
   the release tag/commit, and add them to the AppStream metadata.
6. Run `packaging/check-distribution.sh --flathub-ready`,
   `flatpak-builder-lint manifest`, `flatpak-builder-lint appstream`, a clean
   build with `org.flatpak.Builder`, and a sandboxed functional smoke test.
7. Obtain an explicit written eligibility decision from Flathub **before
   opening a pull request**. As of 2026-07-13, Flathub's Requirements prohibit
   AI-assisted application content and AI-generated submission material,
   review comments, and replies, with exceptions only at Flathub's discretion
   for mature, well-maintained projects. Consilium currently contains
   AI-assisted work and has not established the release history that policy
   expects. This file, the manifest, and automation must not be copied into a
   submission unless Flathub says they are eligible.
8. If Flathub grants an exception, the human owner must independently prepare
   and submit the pull request, respond to reviewers without AI-generated
   text, enable GitHub 2FA, accept the Flathub repository invitation, and
   complete Flathub verification. Reviewer approval and store publication are
   external decisions; repository automation cannot promise them.

Until those gates are satisfied, distribute the tested AppImage, deb, and RPM
from the draft-to-public release workflow and describe Flathub as planned,
not available.

## Snap Store

Consilium does not currently ship a Snap recipe or claim a Snap Store listing.
A strictly confined package could expose the local OpenAI-compatible and
direct API routes, but it could not transparently use arbitrary provider CLIs
installed on the host. That route needs a dedicated recipe, complete WebKitGTK
runtime staging, correct desktop interfaces, and functional tests of session
storage and every advertised provider inside the built package.

A package that preserves the host-CLI routes would likely need classic
confinement. The Store requires a separate human review and publisher vetting
before a classic package can be distributed, and relying on host-installed
software is not by itself an accepted justification. Consilium therefore must
not assume that route will be approved.

The practical sequence is:

1. Publish and clean-machine test the normal GitHub release first.
2. Have the owner create a Snapcraft account, register the intended package
   name, and choose either the reduced strict route set or a justified classic
   design.
3. Build the recipe without credentials, test installation and all advertised
   features on multiple supported distributions, and upload to a non-stable
   channel.
4. Complete the Store's automated checks and any required human review. For a
   classic design, obtain approval before attempting a stable release.
5. Promote only the reviewed revision, then re-install it from the Store on a
   separate test machine before advertising availability.

Until those steps are complete, Snap remains an evaluated distribution route,
not a download option.

## Official references

- [Tauri distribution and signing overview](https://v2.tauri.app/distribute/)
- [Tauri Windows installer formats](https://v2.tauri.app/distribute/windows-installer/)
- [Tauri AppImage compatibility guidance](https://v2.tauri.app/distribute/appimage/)
- [Tauri RPM packaging and signing](https://v2.tauri.app/distribute/rpm/)
- [Tauri platform-specific configuration](https://v2.tauri.app/reference/config/#platform-specific-configuration)
- [Official Tauri GitHub Action](https://github.com/tauri-apps/tauri-action)
- [Flathub requirements](https://docs.flathub.org/docs/for-app-authors/requirements)
- [Flathub submission process](https://docs.flathub.org/docs/for-app-authors/submission)
- [Flathub MetaInfo guidelines](https://docs.flathub.org/docs/for-app-authors/metainfo-guidelines)
- [Flathub screenshot quality guidelines](https://docs.flathub.org/docs/for-app-authors/metainfo-guidelines/quality-guidelines#screenshots)
- [Flatpak manifest documentation](https://docs.flatpak.org/en/latest/manifests.html)
- [AppStream MetaInfo specification](https://www.freedesktop.org/software/appstream/docs/chap-Metadata.html)
- [AppImage distribution and AppImageHub submission](https://docs.appimage.org/packaging-guide/distribution.html)
- [Snap publishing workflow](https://snapcraft.io/docs/releasing-your-app/)
- [Snap classic-review process](https://snapcraft.io/docs/reference/administration/reviewing-classic-confinement-snaps/)
