#!/usr/bin/env bash
# Exercise the packaged WebKitGTK frontend against the synthetic LAN fixture.
set -euo pipefail
if [[ "${1:-}" != --inside ]]; then
  exec xvfb-run -a -s '-screen 0 1600x1000x24 -nolisten tcp' dbus-run-session -- bash "$0" --inside
fi
export XDG_SESSION_TYPE=x11 GDK_BACKEND=x11 GTK_USE_PORTAL=0 WEBKIT_DISABLE_COMPOSITING_MODE=1
unset WAYLAND_DISPLAY
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
cargo build -p vnt-remote --example android_fixture --locked
"$root/target/debug/examples/android_fixture" > "$test_dir/fixture.log" 2>&1 &
fixture_pid=$!
mkdir -p "$root/target/linux-smoke" "$test_dir/deb" "$test_dir/image"
dpkg-deb -x "$root"/target/release/bundle/deb/*_amd64.deb "$test_dir/deb"
(cd "$test_dir/image" && "$root"/target/release/bundle/appimage/*_amd64.AppImage --appimage-extract >/dev/null)
for package in deb appimage; do
  export XDG_DATA_HOME="$test_dir/$package-data" XDG_CONFIG_HOME="$test_dir/$package-config"
  if [[ "$package" == deb ]]; then binary="$test_dir/deb/usr/bin/vnt-remote";
  else binary="$test_dir/image/squashfs-root/AppRun"; fi
  "$binary" > "$root/target/linux-smoke/$package.log" 2>&1 &
  app_pid=$!
  main=
  for _ in {1..120}; do
    kill -0 "$app_pid"
    main=$(xdotool search --onlyvisible --name '^VNT Remote$' | head -1 || true)
    [[ -z "$main" ]] || break
    sleep 0.5
  done
  [[ -n "$main" ]] || { cat "$root/target/linux-smoke/$package.log"; exit 1; }
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
  python3 scripts/linux-smoke-image.py "$root/target/linux-smoke/$package-first.png"
  sleep 1
  python3 scripts/linux-smoke-image.py "$root/target/linux-smoke/$package.png" "$root/target/linux-smoke/$package-first.png"
  kill "$app_pid"
  wait "$app_pid" || true
  app_pid=
  echo "Verified $package: live H.264 video in packaged WebKitGTK"
done
