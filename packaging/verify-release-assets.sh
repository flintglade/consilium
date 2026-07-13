#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

assets_dir="${1:-}"
[[ -n "$assets_dir" ]] || {
  printf 'usage: %s RELEASE_ASSETS_DIRECTORY\n' "$0" >&2
  exit 64
}
[[ -d "$assets_dir" ]] || {
  printf 'release artifact check failed: directory does not exist: %s\n' "$assets_dir" >&2
  exit 1
}
assets_dir="$(cd "$assets_dir" && pwd)"

fail() {
  printf 'release artifact check failed: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null || fail "$1 is required"
}

single_asset() {
  local label="$1"
  local pattern="$2"
  local -a matches=()
  mapfile -d '' matches < <(find "$assets_dir" -maxdepth 1 -type f -iname "$pattern" -print0)
  ((${#matches[@]} == 1)) ||
    fail "expected one $label matching $pattern, found ${#matches[@]}"
  printf '%s\n' "${matches[0]}"
}

require_command 7z
require_command appstreamcli
require_command cpio
require_command dpkg-deb
require_command file
require_command jq
require_command msiextract
require_command msiinfo
require_command objdump
require_command rpm
require_command rpm2cpio
require_command sha256sum
require_command unzip

appimage="$(single_asset 'AppImage' '*.AppImage')"
deb="$(single_asset 'Debian package' '*.deb')"
rpm_package="$(single_asset 'RPM package' '*.rpm')"
nsis="$(single_asset 'NSIS installer' '*.exe')"
msi="$(single_asset 'MSI installer' '*.msi')"
portable="$(single_asset 'Windows portable ZIP' '*portable*.zip')"

for artifact in "$appimage" "$deb" "$rpm_package" "$nsis" "$msi" "$portable"; do
  [[ -s "$artifact" ]] || fail "artifact is empty: $artifact"
done

version="$(jq -r '.version' desktop/src-tauri/tauri.conf.json)"
homepage="https://flintglade.com/"
linux_publisher="Flintglade <support@flintglade.com>"
work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT

compare_license() {
  local bundled_license="$1"
  local label="$2"
  [[ -f "$bundled_license" ]] || fail "$label does not contain its license file"
  cmp --silent LICENSE "$bundled_license" || fail "$label license does not match LICENSE"
}

compare_notices() {
  local bundled_notices="$1"
  local label="$2"
  [[ -f "$bundled_notices" ]] || fail "$label does not contain THIRD_PARTY_NOTICES.txt"
  cmp --silent THIRD_PARTY_NOTICES.txt "$bundled_notices" ||
    fail "$label third-party notices do not match THIRD_PARTY_NOTICES.txt"
}

verify_windows_x86_64_executable() {
  local executable="$1"
  local label="$2"
  local identity
  [[ -s "$executable" ]] || fail "$label executable is missing or empty"
  identity="$(file -b "$executable")"
  [[ "$identity" == *"PE32+"* && "$identity" == *"x86-64"* ]] ||
    fail "$label is not an x86-64 PE32+ executable: $identity"
  objdump -f "$executable" | grep -Fq 'architecture: i386:x86-64' ||
    fail "$label COFF header does not identify x86-64"
}

single_payload_file() {
  local root="$1"
  local label="$2"
  shift 2
  local -a matches=()
  local pattern
  for pattern in "$@"; do
    while IFS= read -r -d '' match; do
      matches+=("$match")
    done < <(find "$root" -type f -iname "$pattern" -print0)
  done
  ((${#matches[@]} == 1)) ||
    fail "expected one $label in the extracted payload, found ${#matches[@]}"
  printf '%s\n' "${matches[0]}"
}

deb_version="$(dpkg-deb --field "$deb" Version)"
deb_homepage="$(dpkg-deb --field "$deb" Homepage)"
deb_maintainer="$(dpkg-deb --field "$deb" Maintainer)"
[[ "$deb_version" == "$version" ]] ||
  fail "Debian package version is $deb_version, expected $version"
[[ "$deb_homepage" == "$homepage" ]] ||
  fail "Debian Homepage is $deb_homepage, expected $homepage"
[[ "$deb_maintainer" == "$linux_publisher" ]] ||
  fail "Debian Maintainer is $deb_maintainer, expected $linux_publisher"
dpkg-deb --extract "$deb" "$work_dir/deb"
compare_license "$work_dir/deb/usr/share/doc/consilium/copyright" 'Debian package'
compare_notices \
  "$work_dir/deb/usr/share/doc/consilium/THIRD_PARTY_NOTICES.txt" \
  'Debian package'

rpm_version="$(rpm --query --package --queryformat '%{VERSION}' "$rpm_package")"
rpm_homepage="$(rpm --query --package --queryformat '%{URL}' "$rpm_package")"
[[ "$rpm_version" == "$version" ]] ||
  fail "RPM version is $rpm_version, expected $version"
[[ "$rpm_homepage" == "$homepage" ]] ||
  fail "RPM URL is $rpm_homepage, expected $homepage"
mkdir "$work_dir/rpm"
rpm_archive="$work_dir/package.cpio"
if ! rpm2cpio "$rpm_package" >"$rpm_archive"; then
  [[ -s "$rpm_archive" ]] || fail 'could not read the RPM payload'
fi
(
  cd "$work_dir/rpm"
  cpio --extract --make-directories --quiet <"$rpm_archive"
)
compare_license "$work_dir/rpm/usr/share/licenses/consilium/LICENSE" 'RPM package'
compare_notices \
  "$work_dir/rpm/usr/share/licenses/consilium/THIRD_PARTY_NOTICES.txt" \
  'RPM package'

chmod u+x "$appimage"
mkdir "$work_dir/appimage"
(
  cd "$work_dir/appimage"
  "$appimage" --appimage-extract >/dev/null
)
compare_license \
  "$work_dir/appimage/squashfs-root/usr/share/licenses/consilium/LICENSE" \
  'AppImage'
compare_notices \
  "$work_dir/appimage/squashfs-root/usr/share/licenses/consilium/THIRD_PARTY_NOTICES.txt" \
  'AppImage'
appimage_meta="$work_dir/appimage/squashfs-root/usr/share/metainfo/com.flintglade.consilium.metainfo.xml"
[[ -s "$appimage_meta" ]] || fail 'AppImage does not contain AppStream metadata'
appstreamcli validate --no-net "$appimage_meta" >/dev/null ||
  fail 'AppImage AppStream metadata is invalid'

printf '%s\n' \
  'Consilium.exe' \
  'LICENSE.txt' \
  'README.txt' \
  'THIRD_PARTY_NOTICES.txt' \
  >"$work_dir/portable-expected"
unzip -Z1 "$portable" | sed '/^$/d' | LC_ALL=C sort >"$work_dir/portable-actual"
if ! cmp --silent "$work_dir/portable-expected" "$work_dir/portable-actual"; then
  diff -u "$work_dir/portable-expected" "$work_dir/portable-actual" >&2 || true
  fail 'portable ZIP contents do not exactly match the four reviewed root files'
fi
unzip -p "$portable" LICENSE.txt >"$work_dir/portable-license"
compare_license "$work_dir/portable-license" 'portable ZIP'
unzip -p "$portable" THIRD_PARTY_NOTICES.txt >"$work_dir/portable-notices"
compare_notices "$work_dir/portable-notices" 'portable ZIP'
unzip -p "$portable" Consilium.exe >"$work_dir/portable-Consilium.exe"
verify_windows_x86_64_executable \
  "$work_dir/portable-Consilium.exe" \
  'portable Consilium.exe'
unzip -p "$portable" README.txt >"$work_dir/portable-readme"
grep -Fq 'https://flintglade.com/' "$work_dir/portable-readme" ||
  fail 'portable README is missing the Flintglade homepage'
grep -Fq 'https://www.patreon.com/c/zach457' "$work_dir/portable-readme" ||
  fail 'portable README is missing optional project support information'

mkdir "$work_dir/msi"
msiextract -C "$work_dir/msi" "$msi" >/dev/null
msiinfo export "$msi" Property >"$work_dir/msi-properties"
msi_property() {
  local property="$1"
  awk -F '\t' -v property="$property" '
    $1 == property {
      sub(/\r$/, "", $2)
      print $2
      exit
    }
  ' "$work_dir/msi-properties"
}
msi_name="$(msi_property ProductName)"
msi_version="$(msi_property ProductVersion)"
msi_manufacturer="$(msi_property Manufacturer)"
[[ "$msi_name" == 'Consilium' ]] ||
  fail "MSI ProductName is ${msi_name:-<missing>}, expected Consilium"
[[ "$msi_version" == "$version" ]] ||
  fail "MSI ProductVersion is ${msi_version:-<missing>}, expected $version"
[[ "$msi_manufacturer" == 'Flintglade' ]] ||
  fail "MSI Manufacturer is ${msi_manufacturer:-<missing>}, expected Flintglade"
msi_template="$({
  msiinfo suminfo "$msi" |
    awk -F ':' '/^[[:space:]]*Template:/ {
      sub(/^[[:space:]]*/, "", $2)
      sub(/\r$/, "", $2)
      print $2
      exit
    }'
})"
case "$msi_template" in
  x64* | X64* | AMD64* | amd64*) ;;
  *) fail "MSI SummaryInformation Template is ${msi_template:-<missing>}, expected x64" ;;
esac
msi_executable="$(single_payload_file \
  "$work_dir/msi" 'Consilium application executable in MSI' \
  'grok-chat-desktop.exe' 'Consilium.exe')"
verify_windows_x86_64_executable "$msi_executable" 'MSI application payload'
msi_notices="$(single_payload_file \
  "$work_dir/msi" 'THIRD_PARTY_NOTICES.txt in MSI' \
  'THIRD_PARTY_NOTICES.txt')"
compare_notices "$msi_notices" 'MSI package'

mkdir "$work_dir/nsis"
7z l -slt "$nsis" >"$work_dir/nsis-listing"
grep -Fqx 'Type = Nsis' "$work_dir/nsis-listing" ||
  fail 'setup executable is not recognized as an NSIS archive'
7z x -y -o"$work_dir/nsis" "$nsis" >/dev/null
nsis_executable="$(single_payload_file \
  "$work_dir/nsis" 'Consilium application executable in NSIS' \
  'grok-chat-desktop.exe' 'Consilium.exe')"
# NSIS uses its own launcher stub, which can be 32-bit even for a 64-bit
# product. Validate the embedded application payload instead of the stub.
verify_windows_x86_64_executable "$nsis_executable" 'NSIS application payload'
nsis_notices="$(single_payload_file \
  "$work_dir/nsis" 'THIRD_PARTY_NOTICES.txt in NSIS' \
  'THIRD_PARTY_NOTICES.txt')"
compare_notices "$nsis_notices" 'NSIS package'

printf 'release artifact checks passed for Consilium %s (6 artifacts)\n' "$version"
