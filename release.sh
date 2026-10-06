#!/usr/bin/env bash
# release.sh — build Mud Hut's release file and print the command that publishes it.
#
#   ./release.sh        -> dist/mudhut + dist/SHA256SUMS
#
# Builds a fresh clone of HEAD (commit first) under /var/tmp, so the binary carries no path from
# this machine: rustc embeds source paths for panic messages, and --remap-path-prefix rewrites the
# clone's and cargo's paths to neutral ones. The runtime tools (hdpim_host.exe,
# extract_accc_runtime.py) are compiled in, so the release is this one file. `neutron setup`
# downloads it and checks it against SHA256SUMS.
set -euo pipefail

REPO="$(cd "$(dirname "$0")" && pwd)"
WORK="${MUDHUT_RELEASE_WORK:-/var/tmp/mudhut-release}"
CARGO_DIR="${CARGO_HOME:-$HOME/.cargo}"
die() { echo "release.sh: $*" >&2; exit 1; }

git -C "$REPO" diff --quiet HEAD || die "uncommitted changes; the release is built from HEAD"
VER="$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO/Cargo.toml" | head -1)"
[ -n "$VER" ] || die "no version in Cargo.toml"

rm -rf "$WORK"; mkdir -p "$WORK"
git clone -q "$REPO" "$WORK/src"
cd "$WORK/src"
RUSTFLAGS="--remap-path-prefix=$WORK/src=/build/mud-hut --remap-path-prefix=$CARGO_DIR=/cargo" \
    cargo build --release --locked
BIN="$WORK/src/target/release/mudhut"

# Gates: no home path in the binary; runs on the glibc floor (2.39: Ubuntu 24.04, Fedora 40).
if strings -a "$BIN" | grep -qF "$HOME/"; then
    die "the binary contains $HOME/"
fi
NEWEST="$(objdump -T "$BIN" | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1)"
[ "$(printf '%s\nGLIBC_2.39\n' "$NEWEST" | sort -V | tail -1)" = "GLIBC_2.39" ] \
    || die "needs $NEWEST, newer than the glibc 2.39 floor"

mkdir -p "$REPO/dist"
cp "$BIN" "$REPO/dist/mudhut"
( cd "$REPO/dist" && sha256sum mudhut > SHA256SUMS )
echo
echo "Built Mud Hut $VER ($("$REPO/dist/mudhut" --version)), needs up to $NEWEST."
echo "Publish:"
echo "  git tag v$VER && git push origin v$VER"
echo "  gh release create v$VER dist/mudhut dist/SHA256SUMS --title \"Mud Hut $VER\" --notes \"...\""
