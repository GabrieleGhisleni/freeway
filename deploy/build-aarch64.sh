#!/usr/bin/env bash
# Cross-compile the Rust binary for a 64-bit Raspberry Pi (aarch64) inside Docker.
# Output: rust/target/aarch64-unknown-linux-gnu/release/freeway
# Needs only Docker on the machine that runs it (PC or CI); nothing is built on the Pi.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET=aarch64-unknown-linux-gnu

# The image is Debian bookworm (older glibc than the Pi's), so the binary runs on newer Debian too.
# Cargo registry lives in a named volume so repeated builds are fast.
docker run --rm \
  -v "$ROOT/rust:/src" -w /src \
  -v freeway-cargo-registry:/usr/local/cargo/registry \
  -e CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
  -e CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
  rust:1-bookworm bash -c "
    set -e
    apt-get update -qq && apt-get install -y -qq gcc-aarch64-linux-gnu >/dev/null
    rustup target add $TARGET
    cargo build --release --locked --target $TARGET
    chown -R $(id -u):$(id -g) /src/target
  "

BIN="$ROOT/rust/target/$TARGET/release/freeway"
file "$BIN" | grep -q aarch64 || { echo "unexpected binary architecture" >&2; exit 1; }
echo "built: $BIN"
