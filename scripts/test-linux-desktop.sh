#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" != --inside ]]; then
  exec xvfb-run -a -s '-screen 0 1280x720x24 -nolisten tcp' dbus-run-session -- bash "$0" --inside
fi
export XDG_SESSION_TYPE=x11
unset WAYLAND_DISPLAY
export XDG_RUNTIME_DIR
XDG_RUNTIME_DIR=$(mktemp -d)
trap 'pulseaudio --kill >/dev/null 2>&1 || true; rm -rf "$XDG_RUNTIME_DIR"' EXIT
pulse_args=()
if [[ -n "${VNT_PULSE_MODULE_PATH:-}" ]]; then pulse_args+=(--dl-search-path="$VNT_PULSE_MODULE_PATH"); fi
cat > "$XDG_RUNTIME_DIR/test.pa" <<'PULSE'
.fail
load-module module-native-protocol-unix
load-module module-null-sink sink_name=vnt_test
set-default-sink vnt_test
PULSE
pulseaudio -n --daemonize --exit-idle-time=-1 "${pulse_args[@]}" --file="$XDG_RUNTIME_DIR/test.pa"
pactl set-default-sink vnt_test
export XDG_DATA_HOME="$XDG_RUNTIME_DIR/data"
export XDG_CONFIG_HOME="$XDG_RUNTIME_DIR/config"
mkdir -p "$XDG_DATA_HOME" "$XDG_CONFIG_HOME"
printf '\n' | gnome-keyring-daemon --unlock --components=secrets
cargo test -p vnt-linux --test session --locked -- --ignored --test-threads=1
cargo test -p vnt-host --test linux_store --locked -- --ignored --test-threads=1
cargo test -p vnt-input --test linux_desktop --locked -- --ignored --test-threads=1
cargo test -p vnt-capture --test linux_desktop --locked -- --ignored --test-threads=1
cargo test -p vnt-clip --test linux_desktop --locked -- --ignored --test-threads=1
cargo test -p vnt-audio --lib --locked linux_native -- --ignored --test-threads=1
cargo test -p vnt-remote --lib --locked linux_native_picker -- --ignored --test-threads=1
cargo test -p vnt-remote --test e2e --locked
