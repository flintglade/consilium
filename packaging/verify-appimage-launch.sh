#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

artifact="${1:-}"
[[ -n "$artifact" ]] || {
  printf 'usage: %s APPIMAGE\n' "$0" >&2
  exit 64
}
artifact="$(realpath "$artifact")"
[[ -s "$artifact" ]] || {
  printf 'AppImage launch check failed: artifact is missing or empty\n' >&2
  exit 1
}

for command in firejail realpath xdotool xvfb-run; do
  command -v "$command" >/dev/null || {
    printf 'AppImage launch check failed: %s is required\n' "$command" >&2
    exit 1
  }
done

if [[ "${CONSILIUM_XVFB_CHILD:-0}" != '1' ]]; then
  export CONSILIUM_XVFB_CHILD=1
  exec xvfb-run -a -s '-screen 0 1280x800x24 -ac -nolisten tcp' "$0" "$artifact"
fi

work_dir="$(mktemp -d)"
app_pid=''
cleanup() {
  if [[ -n "$app_pid" ]]; then
    kill "$app_pid" 2>/dev/null || true
    wait "$app_pid" 2>/dev/null || true
  fi
  rm -rf "$work_dir"
}
trap cleanup EXIT

mkdir -p "$work_dir/home" "$work_dir/data"
HOME="$work_dir/home" \
GROK_CHAT_DATA_DIR="$work_dir/data" \
WEBKIT_DISABLE_COMPOSITING_MODE=1 \
firejail --quiet --noprofile --net=none --appimage "$artifact" \
  >"$work_dir/app.log" 2>&1 &
app_pid=$!

window=''
for _ in $(seq 1 300); do
  window="$(xdotool search --onlyvisible --name '^Consilium$' 2>/dev/null | head -1 || true)"
  [[ -n "$window" ]] && break
  kill -0 "$app_pid" 2>/dev/null || break
  sleep 0.1
done

if [[ -z "$window" ]]; then
  sed -n '1,160p' "$work_dir/app.log" >&2
  printf 'AppImage launch check failed: no Consilium window appeared\n' >&2
  exit 1
fi

window_name="$(xdotool getwindowname "$window")"
[[ "$window_name" == 'Consilium' ]] || {
  printf 'AppImage launch check failed: unexpected window name %s\n' "$window_name" >&2
  exit 1
}

printf 'AppImage offline launch check passed with visible Consilium window\n'
