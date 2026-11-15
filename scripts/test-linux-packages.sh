#!/usr/bin/env bash
# Exercise the packaged WebKitGTK frontend against the synthetic LAN fixture.
set -euo pipefail
if [[ "${1:-}" != --inside ]]; then
  exec xvfb-run -a -s '-screen 0 1600x1000x24 -nolisten tcp' dbus-run-session -- bash "$0" --inside
fi
export XDG_SESSION_TYPE=x11 GDK_BACKEND=x11 GTK_USE_PORTAL=0 WEBKIT_DISABLE_COMPOSITING_MODE=1
unset WAYLAND_DISPLAY
# Xvfb has no DRI3 presentation surface. Verify packaged software decoding;
# hardware encoding has its own strict GPU runner.
export LIBVA_DRIVER_NAME=vnt-smoke-no-gpu
root=$(pwd)
test_dir=$(mktemp -d)
app_pid=
fixture_pid=
cleanup() {
  [[ -z "$app_pid" ]] || kill "$app_pid" 2>/dev/null || true
  [[ -z "$fixture_pid" ]] || kill "$fixture_pid" 2>/dev/null || true
  rm -rf "$test_dir"
}
trap cleanup EXIT
export XDG_RUNTIME_DIR="$test_dir/runtime"
mkdir -m 700 "$XDG_RUNTIME_DIR"
mkdir -p "$root/target/linux-smoke"
cargo build -p vnt-remote --example android_fixture --locked
"$root/target/debug/examples/android_fixture" > "$root/target/linux-smoke/fixture.log" 2>&1 &
fixture_pid=$!
mkdir -p "$root/target/linux-smoke" "$test_dir/deb" "$test_dir/image"
dpkg-deb -x "$root"/target/release/bundle/deb/*_amd64.deb "$test_dir/deb"
(cd "$test_dir/image" && "$root"/target/release/bundle/appimage/*_amd64.AppImage --appimage-extract >/dev/null)
for package in deb appimage; do
 for codec in h264 hevc av1; do
  export XDG_DATA_HOME="$test_dir/$package-$codec-data" XDG_CONFIG_HOME="$test_dir/$package-$codec-config"
  if [[ "$package" == deb ]]; then
    binary="$test_dir/deb/usr/bin/vnt-remote"
    resources="$test_dir/deb/usr/lib/vnt-remote"
  else
    binary="$test_dir/image/squashfs-root/AppRun"
    resources="$test_dir/image/squashfs-root/usr/lib/vnt-remote"
  fi
  python3 - "$resources" <<'PYABI'
import ctypes, sys
from pathlib import Path
root = Path(sys.argv[1])
backend = ctypes.CDLL(str(root / "linux-codec/libvnt-linux-encoder.so"))
assert backend.vnt_hw_abi_version() == 1
for name in ["FFmpeg-LGPL.txt", "libva-COPYING.txt", "nv-codec-headers-LICENSE.txt"]:
    assert (root / "licenses/linux-codec" / name).is_file(), f"Missing license: {name}"
PYABI
  "$binary" > "$root/target/linux-smoke/$package-$codec.log" 2>&1 &
  app_pid=$!
  main=
  for _ in {1..120}; do
    kill -0 "$app_pid"
    main=$(xdotool search --onlyvisible --name '^VNT Remote$' | head -1 || true)
    [[ -z "$main" ]] || break
    sleep 0.5
  done
  [[ -n "$main" ]] || { cat "$root/target/linux-smoke/$package-$codec.log"; exit 1; }
  xdotool windowmove "$main" 0 0 windowfocus "$main"
  sleep 3
  xdotool mousemove --window "$main" 460 181 click 1
  sleep 0.3
  xdotool key ctrl+a
  xdotool type --clearmodifiers --delay 120 '127.0.0.1:39875'
  xdotool mousemove --window "$main" 458 260 click 1
  sleep 0.3
  xdotool key ctrl+a
  xdotool type --clearmodifiers --delay 120 'android-test'
  xdotool mousemove --window "$main" 500 362 click 1
  sleep 0.3
  case "$codec" in h264) codec_x=450 ;; hevc) codec_x=512 ;; av1) codec_x=570 ;; esac
  python3 scripts/linux-smoke-image.py --capture-only "$root/target/linux-smoke/$package-$codec-choice.png"
  if python3 - "$root/target/linux-smoke/$package-$codec-choice.png" "$codec_x" <<'PYCHOICE'
from PIL import Image
import sys
x = int(sys.argv[2])
pixels = list(Image.open(sys.argv[1]).convert("RGB").crop((x-21, 480, x+21, 503)).getdata())
# Disabled codec labels have no dark text; enabled buttons use dark blue text.
# Require visible muted text as well so a blank window cannot count as rejection.
disabled = sum(max(p) < 165 for p in pixels) < 5 and sum(max(p) < 220 for p in pixels) > 10
raise SystemExit(0 if disabled else 1)
PYCHOICE
  then
    [[ "$codec" != h264 ]] || { echo 'H.264 decoder is required' >&2; exit 1; }
    echo "Verified $package: $codec rejected before negotiation (decoder self-test unavailable)"
    kill "$app_pid"
    wait "$app_pid" || true
    app_pid=
    continue
  fi
  xdotool mousemove --window "$main" "$codec_x" 491 click 1
  xdotool mousemove --window "$main" 500 315 click 1
  remote=
  for _ in {1..60}; do
    remote=$(xdotool search --onlyvisible --name '^127.0.0.1 · VNT Remote$' | head -1 || true)
    [[ -z "$remote" ]] || break
    sleep 0.5
  done
  [[ -n "$remote" ]]

  xdotool windowmove "$remote" 0 0 windowraise "$remote"
  sleep 5
  python3 - "$root/target/linux-smoke/fixture.log" "$codec" <<'PYCODE'
import json, sys
from pathlib import Path
sessions = [json.loads(line.removeprefix("fixture-session ")) for line in
            Path(sys.argv[1]).read_text().splitlines() if line.startswith("fixture-session ")]
assert sessions, "The fixture must have a real session"
assert sessions[-1]["codec"] == sys.argv[2], f"Requested {sys.argv[2]}, negotiated {sessions[-1]['codec']}; decoder may be unavailable"
PYCODE
  python3 scripts/linux-smoke-image.py "$root/target/linux-smoke/$package-$codec-first.png"
  sleep 1
  python3 scripts/linux-smoke-image.py "$root/target/linux-smoke/$package-$codec.png" "$root/target/linux-smoke/$package-$codec-first.png"
  kill "$app_pid"
  wait "$app_pid" || true
  app_pid=
  echo "Verified $package: live $codec video in packaged WebKitGTK"
 done
done
