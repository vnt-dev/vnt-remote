#!/usr/bin/env bash
# Decode the real software bitstreams; an empty/invalid stream cannot pass.
set -euo pipefail
mkdir -p target/linux-codec-tests
cargo build -p vnt-codec --bin codec-probe --locked
for codec in h264 hevc av1; do
  case "$codec" in
    h264) parser=h264parse; decoder=avdec_h264 ;;
    hevc) parser=h265parse; decoder=avdec_h265 ;;
    av1) parser=av1parse; decoder=av1dec ;;
  esac
  stream="target/linux-codec-tests/$codec.bin"
  raw="target/linux-codec-tests/$codec.i420"
  CODEC_PROBE_DUMP_STREAM="$stream" target/debug/codec-probe test "$codec" 128x96 soft > "target/linux-codec-tests/$codec.log" 2>&1
  gst-launch-1.0 -q filesrc location="$stream" ! "$parser" ! "$decoder" ! videoconvert ! \
    'video/x-raw,format=I420,width=128,height=96' ! filesink location="$raw"
  python3 - "$raw" <<'PY'
from pathlib import Path
import sys
actual = Path(sys.argv[1]).stat().st_size
assert actual == 128 * 96 * 3 // 2 * 60, f"Expected 60 decoded frames, got {actual} bytes"
PY
  echo "Verified $codec: 60 software frames decoded"
done
