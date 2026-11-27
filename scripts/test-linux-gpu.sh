#!/usr/bin/env bash
# Run only on an explicitly prepared GPU machine. No software fallback is allowed.
set -euo pipefail
codecs=(h264 hevc av1)
if (( $# )); then codecs=("$@"); fi
mkdir -p target/linux-gpu-tests
cargo build -p vnt-codec --bin codec-probe --locked
for codec in "${codecs[@]}"; do
  case "$codec" in h264|hevc|av1) ;; *) echo "Unknown codec: $codec" >&2; exit 1 ;; esac
  CODEC_PROBE_DUMP_STREAM="target/linux-gpu-tests/$codec.bin" \
    target/debug/codec-probe test "$codec" 1280x720 --hardware-only 2>&1 | tee "target/linux-gpu-tests/$codec.log"
  grep -q '硬件=true' "target/linux-gpu-tests/$codec.log"
done
required=$(IFS=,; echo "${codecs[*]}")
VNT_GPU_TEST_CODECS="$required" cargo test -p vnt-codec --lib --locked \
  linux::tests::gpu_keyframes_timestamps_reconfiguration_and_shutdown -- --ignored --nocapture \
  2>&1 | tee target/linux-gpu-tests/lifecycle.log
for codec in "${codecs[@]}"; do
  case "$codec" in
    h264) parser=h264parse; decoder=avdec_h264 ;;
    hevc) parser=h265parse; decoder=avdec_h265 ;;
    av1) parser=av1parse; decoder=av1dec ;;
  esac
  raw="target/linux-gpu-tests/$codec.i420"
  gst-launch-1.0 -q filesrc location="target/linux-gpu-tests/$codec.bin" ! "$parser" ! "$decoder" ! \
    videoconvert ! 'video/x-raw,format=I420,width=1280,height=720' ! filesink location="$raw"
  python3 - "$raw" <<'PY'
from pathlib import Path
import sys
actual = Path(sys.argv[1]).stat().st_size
assert actual == 1280 * 720 * 3 // 2 * 60, f"Expected 60 GPU frames, got {actual} bytes"
PY
  echo "Verified hardware $codec: 60 frames decoded"
done
