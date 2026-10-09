#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

mode="${1:-local}"
tag="${2:-}"
app_id="com.flintglade.consilium"
homepage="https://flintglade.com/"
repository="https://github.com/flintglade/consilium"
patreon="https://www.patreon.com/c/zach457"
linux_publisher="Flintglade <support@flintglade.com>"
meta="packaging/flatpak/${app_id}.metainfo.xml"
appimage_meta="packaging/appimage/${app_id}.metainfo.xml"
desktop="packaging/flatpak/${app_id}.desktop"
flatpak_manifest="packaging/flatpak/${app_id}.local.yml"

fail() {
  printf 'distribution check failed: %s\n' "$*" >&2
  exit 1
}

case "$mode" in
  local | --flathub-ready) ;;
  --release)
    [[ -n "$tag" ]] || fail "--release requires a tag"
    ;;
  *) fail "unknown mode: $mode" ;;
esac

command -v jq >/dev/null || fail "jq is required"
jq -e '.productName == "Consilium" and .identifier == "com.flintglade.consilium"' \
  desktop/src-tauri/tauri.conf.json >/dev/null || fail "Tauri identity mismatch"
jq -e --arg homepage "$homepage" --arg publisher "$linux_publisher" '
  .bundle.active == true and
  .bundle.targets == ["appimage", "deb", "rpm"] and
  .bundle.publisher == $publisher and
  .bundle.homepage == $homepage and
  .bundle.license == "Apache-2.0" and
  .bundle.licenseFile == "../../LICENSE" and
  .bundle.linux.appimage.files["/usr/share/licenses/consilium/LICENSE"] == "../../LICENSE" and
  .bundle.linux.appimage.files["/usr/share/licenses/consilium/THIRD_PARTY_NOTICES.txt"] == "../../THIRD_PARTY_NOTICES.txt" and
  .bundle.linux.appimage.files["/usr/share/metainfo/com.flintglade.consilium.metainfo.xml"] == "../../packaging/appimage/com.flintglade.consilium.metainfo.xml" and
  .bundle.linux.deb.files["/usr/share/doc/consilium/copyright"] == "../../LICENSE" and
  .bundle.linux.deb.files["/usr/share/doc/consilium/THIRD_PARTY_NOTICES.txt"] == "../../THIRD_PARTY_NOTICES.txt" and
  .bundle.linux.rpm.files["/usr/share/licenses/consilium/LICENSE"] == "../../LICENSE" and
  .bundle.linux.rpm.files["/usr/share/licenses/consilium/THIRD_PARTY_NOTICES.txt"] == "../../THIRD_PARTY_NOTICES.txt"
' desktop/src-tauri/tauri.linux.conf.json >/dev/null ||
  fail "Linux bundle metadata, publisher, or notice payload mapping is incomplete"
jq -e --arg homepage "$homepage" '
  .bundle.active == true and
  .bundle.targets == ["nsis", "msi"] and
  .bundle.publisher == "Flintglade" and
  .bundle.homepage == $homepage and
  .bundle.license == "Apache-2.0" and
  .bundle.licenseFile == "../../LICENSE" and
  .bundle.resources == {"../../THIRD_PARTY_NOTICES.txt": "THIRD_PARTY_NOTICES.txt"}
' desktop/src-tauri/tauri.windows.conf.json >/dev/null ||
  fail "Windows bundle metadata or notice resource mapping is incomplete"

version="$(jq -r '.version' desktop/src-tauri/tauri.conf.json)"
for manifest in core/Cargo.toml tui/Cargo.toml desktop/src-tauri/Cargo.toml; do
  package_version="$(awk -F '"' '/^version = "/ { print $2; exit }' "$manifest")"
  [[ "$package_version" == "$version" ]] || fail "$manifest is $package_version, expected $version"
done
cargo metadata --locked --no-deps --format-version 1 |
  jq -e --arg homepage "$homepage" --arg repository "$repository" '
    . as $metadata |
    ($metadata.packages | length == 3) and
    all($metadata.packages[];
      .authors == ["Flintglade <support@flintglade.com>"] and
      .homepage == $homepage and
      .repository == $repository and
      .license == "Apache-2.0"
    )
  ' >/dev/null || fail "Cargo package publisher metadata is incomplete"

if [[ "$mode" == "--release" ]]; then
  [[ "$tag" == "v$version" ]] || fail "tag must be v$version, got ${tag:-<empty>}"
fi

command -v desktop-file-validate >/dev/null || fail "desktop-file-validate is required"
desktop-file-validate "$desktop"

command -v appstreamcli >/dev/null || fail "appstreamcli is required"
appstreamcli validate --no-net "$meta"
appstreamcli validate --no-net "$appimage_meta"

[[ -s desktop/src-tauri/icons/icon.png ]] || fail "Linux icon is missing"
[[ -s desktop/src-tauri/icons/icon.ico ]] || fail "Windows icon is missing"
[[ -s desktop/icons/consilium.svg ]] || fail "scalable Flintglade icon is missing"
[[ -s packaging/flatpak/cargo-sources.json ]] || fail "offline Cargo sources are missing"
python3 - <<'PY'
import json
import tomllib
from pathlib import Path

packages = tomllib.loads(Path("Cargo.lock").read_text())["package"]
expected = {
    f"https://static.crates.io/crates/{p['name']}/{p['name']}-{p['version']}.crate": p["checksum"]
    for p in packages if p.get("source", "").startswith("registry+")
}
sources = json.loads(Path("packaging/flatpak/cargo-sources.json").read_text())
actual = {s["url"]: s.get("sha256") for s in sources
          if s.get("url", "").startswith("https://static.crates.io/crates/")}
if actual != expected:
    raise SystemExit("offline Flatpak sources differ from Cargo.lock; regenerate cargo-sources.json")
PY
[[ -s LICENSE ]] || fail "Apache-2.0 license file is missing"
[[ -s THIRD_PARTY_NOTICES.txt ]] || fail "third-party notices are missing"
[[ -s packaging/attribution-supplements.json ]] ||
  fail "reviewed attribution supplement manifest is missing"
[[ -x packaging/generate-third-party-notices.sh ]] ||
  fail "third-party notice generator is not executable"
[[ -x packaging/verify-release-assets.sh ]] ||
  fail "release artifact verifier is not executable"
[[ -x packaging/prepare-tauri-apprun.sh ]] ||
  fail "Tauri AppRun preparation helper is not executable"
[[ -x packaging/verify-appimage-launch.sh ]] ||
  fail "AppImage offline launch verifier is not executable"
grep -Fxq 'THIRD_PARTY_NOTICES.txt -text' .gitattributes ||
  fail "third-party notice bytes are not protected from checkout conversion"
grep -Fxq 'packaging/license-fallbacks/* -text' .gitattributes ||
  fail "fallback material bytes are not protected from checkout conversion"
grep -Fxq 'packaging/attribution-supplements/* -text' .gitattributes ||
  fail "attribution supplement bytes are not protected from checkout conversion"
grep -Fxq 'LICENSE text eol=lf' .gitattributes ||
  fail "project license checkout line endings are not fixed"
packaging/generate-third-party-notices.sh --check

grep -Fq "<id>${app_id}</id>" "$meta" || fail "AppStream ID mismatch"
grep -Fq '<project_license>Apache-2.0</project_license>' "$meta" ||
  fail "AppStream license mismatch"
grep -Fq "Icon=${app_id}" "$desktop" || fail "desktop icon ID mismatch"
grep -Fq "<url type=\"homepage\">${homepage}</url>" "$meta" ||
  fail "AppStream homepage mismatch"
grep -Fq "<url type=\"vcs-browser\">${repository}</url>" "$meta" ||
  fail "AppStream repository URL mismatch"
grep -Fq "<url type=\"donation\">${patreon}</url>" "$meta" ||
  fail "Patreon URL mismatch"
grep -Eq '^[[:space:]]+- \.env$' "$flatpak_manifest" ||
  fail "Flatpak source excludes must omit .env"
grep -Eq '^[[:space:]]+- \.env\.\*$' "$flatpak_manifest" ||
  fail "Flatpak source excludes must omit .env.* files"
grep -Fq 'install -Dm0644 THIRD_PARTY_NOTICES.txt /app/share/licenses/com.flintglade.consilium/THIRD_PARTY_NOTICES.txt' \
  "$flatpak_manifest" || fail "Flatpak does not install third-party notices"

grep -Fq "$patreon" .github/FUNDING.yml || fail "GitHub funding link mismatch"
grep -Fq "$patreon" README.md || fail "README is missing the Patreon link"
grep -Fq "$repository" README.md || fail "README is missing the canonical repository link"
grep -Fq 'Windows x86_64' README.md || fail "README is missing Windows distribution guidance"
grep -Fq 'AppImageHub' docs/DISTRIBUTION.md || fail "AppImageHub route is undocumented"
grep -Fq 'Snap Store' docs/DISTRIBUTION.md || fail "Snap Store route is undocumented"

while IFS= read -r workflow; do
  while IFS= read -r uses_line; do
    action_ref="${uses_line#*uses: }"
    action_ref="${action_ref%% *}"
    [[ "$action_ref" == ./* || "$action_ref" =~ ^[^@]+@[0-9a-f]{40}$ ]] ||
      fail "$workflow has a mutable action reference: $action_ref"
  done < <(grep -E '^[[:space:]]*-?[[:space:]]+uses:' "$workflow")
done < <(find .github/workflows -maxdepth 1 -type f \( -name '*.yml' -o -name '*.yaml' \) | sort)

grep -Fq "Copy-Item \"LICENSE\" (Join-Path \$portableRoot \"LICENSE.txt\")" \
  .github/workflows/release-desktop.yml || fail "portable workflow does not include LICENSE.txt"
grep -Fq "Copy-Item \"THIRD_PARTY_NOTICES.txt\" (Join-Path \$portableRoot \"THIRD_PARTY_NOTICES.txt\")" \
  .github/workflows/release-desktop.yml || fail "portable workflow does not include third-party notices"
grep -Fq 'packaging/verify-release-assets.sh release-assets' \
  .github/workflows/release-desktop.yml || fail "release artifacts are not inspected before checksums"
grep -Fq 'run: packaging/prepare-tauri-apprun.sh' \
  .github/workflows/release-desktop.yml || fail "release workflow does not normalize AppImage launch permissions"
grep -Fq "expected=\"release-assets/consilium-\${version}-x86_64.AppImage\"" \
  .github/workflows/release-desktop.yml || fail "release workflow does not normalize the AppImage filename"
grep -Fq 'run: packaging/verify-appimage-launch.sh release-assets/*.AppImage' \
  .github/workflows/release-desktop.yml || fail "release workflow does not run the offline AppImage launch gate"
grep -Fq 'rm -f release-assets/SHA256SUMS SHA256SUMS' \
  .github/workflows/release-desktop.yml || fail "release reruns do not remove old checksums"
grep -Fq "! -name SHA256SUMS" .github/workflows/release-desktop.yml ||
  fail "checksum generation does not exclude SHA256SUMS"
grep -Fq 'runs-on: windows-latest' .github/workflows/ci.yml ||
  fail "normal CI does not run on Windows"
grep -Fq 'cargo test --locked --workspace' .github/workflows/ci.yml ||
  fail "normal CI does not test the Rust workspace"
grep -Fq 'for test in desktop/ui/*.test.js' .github/workflows/ci.yml ||
  fail "normal CI does not test frontend modules"

if [[ "$mode" == "--flathub-ready" ]]; then
  blockers=0
  for screenshot in consilium-main.png consilium-routing.png consilium-recovery.png; do
    path="packaging/screenshots/$screenshot"
    if [[ ! -s "$path" ]]; then
      printf 'BLOCKED: approved Linux store screenshot %s is not present.\n' "$path" >&2
      blockers=1
      continue
    fi
    if ! command -v identify >/dev/null; then
      printf 'BLOCKED: ImageMagick identify is required for screenshot QA.\n' >&2
      blockers=1
      continue
    fi
    read -r width height format < <(identify -format '%w %h %m' "$path")
    if [[ "$format" != "PNG" || "$width" -gt 1000 || "$height" -gt 700 ]]; then
      printf 'BLOCKED: %s must be PNG and no larger than 1000x700 (got %sx%s %s).\n' \
        "$path" "$width" "$height" "$format" >&2
      blockers=1
    fi
  done
  if ! grep -Fq '<screenshots>' "$meta"; then
    printf 'BLOCKED: AppStream has no immutable hosted screenshot URLs.\n' >&2
    blockers=1
  fi
  if ! git remote get-url origin >/dev/null 2>&1; then
    printf 'BLOCKED: no reachable canonical source remote is configured.\n' >&2
    blockers=1
  fi
  printf 'BLOCKED: a human owner must obtain a written Flathub generative-AI policy exception and prepare the submission without AI assistance.\n' >&2
  blockers=1
  (( blockers == 0 )) || exit 2
fi

printf 'distribution checks passed for Consilium %s (%s)\n' "$version" "$mode"
