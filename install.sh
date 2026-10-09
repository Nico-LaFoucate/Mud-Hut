#!/usr/bin/env bash
# install.sh — build and install Mud Hut for real (no symlink-to-a-stale-debug-build).
#
# Builds the release binary and installs it WITH its runtime tools co-located, in
# the shipped layout `repo_tools_dir()` resolves first (`<exe dir>/tools`):
#
#   <install dir>/mudhut         the release binary
#   <install dir>/tools/         hdpim_host.exe, extract_accc_runtime.py
#   <bin dir>/mudhut             symlink to the binary (for PATH)
#
# The bin-dir symlink is safe: the binary locates its tools via /proc/self/exe,
# which the kernel fully resolves, so `<exe dir>` is always the real install dir.
#
# Env overrides:
#   MUDHUT_INSTALL_DIR   install root (default: ${XDG_DATA_HOME:-~/.local/share}/mudhut)
#   MUDHUT_BIN_DIR       symlink dir  (default: ~/.local/bin)
#
# Usage:
#   ./install.sh               build + install + link
#   ./install.sh --uninstall   remove the install dir and the symlink
set -euo pipefail

REPO="$(cd "$(dirname "$0")" && pwd)"
SHARE="${MUDHUT_INSTALL_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/mudhut}"
BIN="${MUDHUT_BIN_DIR:-$HOME/.local/bin}"
# The tools the binary needs at runtime (`--method download` install engine).
RUNTIME_TOOLS=(hdpim_host.exe extract_accc_runtime.py)

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$BIN/mudhut"
    rm -rf "$SHARE"
    echo "uninstalled: $BIN/mudhut and $SHARE"
    exit 0
fi

echo "==> building release binary"
cargo build --release --manifest-path "$REPO/Cargo.toml"

echo "==> installing to $SHARE"
install -Dm755 "$REPO/target/release/mudhut" "$SHARE/mudhut"
for t in "${RUNTIME_TOOLS[@]}"; do
    install -Dm644 "$REPO/tools/$t" "$SHARE/tools/$t"
done

# Optional: stage Adobe's ACCCx packages beside the binary. Mud Hut does not need this:
# without them it downloads Adobe's Creative Cloud package and checks its md5 on the
# first install (or up front with `mudhut accc`). When the folder below exists
# (MUDHUT_ACCC_SRC, or the old development default), all five sets are copied:
# AAM (AdobeApplicationManager), ACC/ACC64 (the Creative Cloud Desktop app itself)
# and ADC/ADC64 (Adobe Desktop Common).
ACCC_SRC="${MUDHUT_ACCC_SRC:-$HOME/mudhut-parent-stage/packages}"
if [ -d "$ACCC_SRC" ]; then
    echo "==> staging the ACCCx runtime packages (~318 MB)"
    rm -rf "$SHARE/accc-packages"
    mkdir -p "$SHARE/accc-packages"
    for _s in AAM ACC ACC64 ADC ADC64 ApplicationInfo.xml; do
        [ -e "$ACCC_SRC/$_s" ] || { echo "!! missing $ACCC_SRC/$_s"; exit 1; }
        cp -a "$ACCC_SRC/$_s" "$SHARE/accc-packages/"
    done
else
    echo "==> Adobe's Creative Cloud package (ACCCx) will be downloaded on the first install"
    echo "    (or now: mudhut accc)"
fi

mkdir -p "$BIN"
ln -sfn "$SHARE/mudhut" "$BIN/mudhut"
echo "==> linked $BIN/mudhut -> $SHARE/mudhut"

case ":$PATH:" in
    *":$BIN:"*) : ;;
    *) echo "note: $BIN is not on your PATH — add it to run 'mudhut' directly" ;;
esac

echo "==> installed: $("$SHARE/mudhut" --version)"
echo "    sanity-check the host with: mudhut doctor"
