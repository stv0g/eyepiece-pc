#!/bin/sh
# Build the FFmpeg libraries eyepiece needs (shared, no programs) into a prefix, for the
# AppImage: an older build base (glibc) has an older FFmpeg than the one the bindings are
# written for, so FFmpeg is built from source and bundled.
#
#   packaging/linux/build-ffmpeg.sh PREFIX
#   then: FFMPEG_INCLUDE_DIR=PREFIX/include FFMPEG_LIB_DIR=PREFIX/lib LD_LIBRARY_PATH=PREFIX/lib
#
# Needs: git, a C toolchain, nasm, pkg-config, libx264-dev, libva-dev, libdrm-dev.
# Encoders: libx264, VAAPI, NVENC (loads the NVIDIA driver's libnvidia-encode at run time).
# AMF and QSV are not built in (they need extra SDK headers / libvpl).
set -eu
FFMPEG_TAG="${FFMPEG_TAG:-n9.0.2}"
# oldest headers FFmpeg 9 accepts in the 12.1 series: NVIDIA driver >= 530 (Linux)
NVHDR_TAG="${NVHDR_TAG:-n12.1.14.0}"
PREFIX="$(mkdir -p "${1:?usage: $0 PREFIX}" && cd "$1" && pwd)"
SRC="${SRC:-$PREFIX/src}"
mkdir -p "$SRC"
JOBS="$(nproc 2>/dev/null || echo 4)"

[ -d "$SRC/nv-codec-headers" ] || git -c advice.detachedHead=false clone -q --depth 1 --branch "$NVHDR_TAG" https://github.com/FFmpeg/nv-codec-headers "$SRC/nv-codec-headers"
make -C "$SRC/nv-codec-headers" -s install PREFIX="$PREFIX"

[ -d "$SRC/ffmpeg" ] || git -c advice.detachedHead=false clone -q --depth 1 --branch "$FFMPEG_TAG" https://github.com/FFmpeg/FFmpeg "$SRC/ffmpeg"
cd "$SRC/ffmpeg"
PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}" ./configure \
    --prefix="$PREFIX" --enable-shared --disable-static \
    --disable-programs --disable-doc --disable-debug \
    --enable-gpl --enable-libx264 \
    --disable-autodetect --enable-pthreads \
    --enable-libdrm --enable-vaapi --enable-ffnvcodec --enable-nvenc
make -s -j"$JOBS"
make -s install
echo "FFmpeg $FFMPEG_TAG in $PREFIX"
