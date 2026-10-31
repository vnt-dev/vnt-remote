# libopus 1.6.1

Upstream: https://opus-codec.org/downloads/

Release archive: https://downloads.xiph.org/releases/opus/opus-1.6.1.tar.gz

SHA-256: `6ffcb593207be92584df15b32466ed64bbec99109f007c82205f0194572411a1`

Retrieved from the NJU Gentoo distfiles mirror because the upstream download
redirect was unreachable on the development host. The archive was checked against
the SHA-256 published by the upstream Opus downloads page before extraction.

The upstream source is unmodified. `crates/vnt-audio/build.rs` builds only the
static codec library with CMake, using the dynamic MSVC runtime to match Rust.
Programs, upstream tests, DRED, OSCE and deep PLC are disabled. Runtime SIMD
dispatch remains enabled; no optional neural model downloads are needed.

`COPYING` contains the upstream license and is included in the application bundle.
