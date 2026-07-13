#!/usr/bin/env bash
set -euo pipefail

# Tauri bundler 2.9.4 writes its cached x86_64 AppRun helper with mode 0770.
# linuxdeploy later renames that helper to AppRun.wrapped inside the AppImage,
# which makes the application unlaunchable for users outside the build UID/GID.
# Seed the exact upstream helper with a portable 0755 mode before bundling.

readonly apprun_url='https://github.com/tauri-apps/binary-releases/releases/download/apprun-old/AppRun-x86_64'
readonly apprun_sha256='f30140a43a0a59e46db21bdefdf749b9e9f2c6946e92afabbacf98b8ae73fb4f'
readonly cache_dir="${TAURI_TOOLS_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/tauri}"
readonly apprun_path="$cache_dir/AppRun-x86_64"

mkdir -p "$cache_dir"

if [[ -f "$apprun_path" ]] &&
  printf '%s  %s\n' "$apprun_sha256" "$apprun_path" | sha256sum --check --status; then
  chmod 0755 "$apprun_path"
else
  download="$(mktemp "$cache_dir/.AppRun-x86_64.XXXXXX")"
  trap 'rm -f "$download"' EXIT
  curl --fail --location --silent --show-error \
    --proto '=https' --tlsv1.2 \
    "$apprun_url" \
    --output "$download"
  printf '%s  %s\n' "$apprun_sha256" "$download" | sha256sum --check --status
  chmod 0755 "$download"
  mv -f "$download" "$apprun_path"
  trap - EXIT
fi

mode="$(stat --format='%a' "$apprun_path")"
[[ "$mode" == '755' ]] || {
  printf 'AppRun preparation failed: mode is %s, expected 755\n' "$mode" >&2
  exit 1
}

printf 'prepared pinned Tauri AppRun helper with portable mode 0755\n'
