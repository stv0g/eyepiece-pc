#!/bin/sh
# Build the portable AppImage like CI does (Ubuntu 24.04, FFmpeg built from source), inside
# a container, so nothing needs to be installed on the host except docker (or podman).
#
#   packaging/linux/build-appimage-docker.sh [WORKDIR]
#
# WORKDIR (default target/appimage-docker) keeps the Rust toolchain, the FFmpeg build,
# linuxdeploy and cargo's target dir between runs. Result: WORKDIR/target/appimage/*.AppImage
set -eu
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
WORK="${1:-$ROOT/target/appimage-docker}"
mkdir -p "$WORK"
WORK="$(cd "$WORK" && pwd)"
DOCKER="${DOCKER:-$(command -v docker || command -v podman)}"
LINUXDEPLOY_VERSION="${LINUXDEPLOY_VERSION:-1-alpha-20251107-1}"
VERSION="${VERSION:-$(git -C "$ROOT" describe --tags --always 2>/dev/null || echo dev)}"

"$DOCKER" run --rm -v "$ROOT:/src:ro" -v "$WORK:/work" \
    -e VERSION="$VERSION" -e LINUXDEPLOY_VERSION="$LINUXDEPLOY_VERSION" \
    -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
    ubuntu:24.04 sh -euc '
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends ca-certificates curl git build-essential \
    pkg-config clang libclang-dev nasm libx264-dev libva-dev libdrm-dev file \
    libpipewire-0.3-dev libspa-0.2-dev >/dev/null
export CARGO_HOME=/work/rust RUSTUP_HOME=/work/rustup PATH=/work/rust/bin:$PATH
[ -x /work/rust/bin/cargo ] || curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal
[ -f /work/ffmpeg/lib/libavcodec.so ] || /src/packaging/linux/build-ffmpeg.sh /work/ffmpeg
LD=/work/linuxdeploy-$LINUXDEPLOY_VERSION-x86_64.AppImage
[ -x "$LD" ] || { curl -sSfL -o "$LD" "https://github.com/linuxdeploy/linuxdeploy/releases/download/$LINUXDEPLOY_VERSION/linuxdeploy-x86_64.AppImage"; chmod +x "$LD"; }
export FFMPEG_INCLUDE_DIR=/work/ffmpeg/include FFMPEG_LIB_DIR=/work/ffmpeg/lib LD_LIBRARY_PATH=/work/ffmpeg/lib
export CARGO_TARGET_DIR=/work/target LINUXDEPLOY="$LD"
/src/packaging/linux/build-appimage.sh
chown -R "$HOST_UID:$HOST_GID" /work
'
ls -l "$WORK"/target/appimage/*.AppImage
