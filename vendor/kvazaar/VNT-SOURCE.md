# Vendored Kvazaar

Unmodified upstream Kvazaar v2.3.2, commit
`6040962bed5cc68c5ad01234c38c08b8b2822068`, downloaded from
https://github.com/ultravideo/kvazaar/archive/refs/tags/v2.3.2.tar.gz.

License: BSD-3-Clause; see LICENSE and the notices in individual source files.
The app bundles LICENSE under licenses/kvazaar-LICENSE.txt.

Cargo builds only the static 8-bit library with CMake; tests, CLI and crypto
are disabled. No network access, submodules, external executables or runtime
DLLs are required by the encoder. The C bridge lives in
crates/vnt-codec/src/kvazaar_bridge.c.

The bridge uses ultrafast, GOP=0 (no B-frame reordering), closed periodic IDRs,
VPS/SPS/PPS on every IDR, lambda rate control, four worker threads and OWF=0.
Non-multiple-of-eight input dimensions are edge-padded and cropped via SPS.
Forced keyframes reopen the encoder because this public API has no per-picture
IDR flag. Rust excludes encoder opens/closes from in-flight encoding because
opens rewrite the global SIMD strategy tables. OWF=0 completes each frame and
its worker jobs before releasing the read lock; sessions can encode in parallel.
