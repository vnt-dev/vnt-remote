#!/usr/bin/env bash
set -euo pipefail
sudo apt-get update
sudo apt-get install -y \
  build-essential cmake nasm meson ninja-build pkg-config curl wget file patchelf \
  libwebkit2gtk-4.1-dev libssl-dev librsvg2-dev libayatana-appindicator3-dev \
  libpulse-dev libxcb1-dev libdrm-dev protobuf-compiler \
  gstreamer1.0-tools gstreamer1.0-libav gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
  xvfb xauth xclip xdotool python3-pil pulseaudio pulseaudio-utils dbus-x11 gnome-keyring
