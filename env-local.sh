# Build without the FFmpeg -dev packages installed: point the build at headers unpacked from
# the .debs and at the runtime libraries. Usage: FFROOT=<dir> . ./env-local.sh
#   apt-get download libavcodec-dev libavutil-dev libavfilter-dev libavformat-dev libswscale-dev libswresample-dev
#   for d in *.deb; do dpkg -x "$d" "$FFROOT/sysroot"; done
#   mkdir -p "$FFROOT/lib"; for l in avcodec.so.63 avfilter.so.12 avformat.so.63 avutil.so.61; do
#     ln -sf /usr/lib/x86_64-linux-gnu/lib$l "$FFROOT/lib/lib${l%%.so*}.so"; done
# (With `sudo apt install libavcodec-dev libavfilter-dev libavformat-dev libavutil-dev` none of this is needed.)
export FFMPEG_INCLUDE_DIR="$FFROOT/sysroot/usr/include/x86_64-linux-gnu"
export FFMPEG_LIB_DIR="$FFROOT/lib"
[ -z "$LIBCLANG_PATH" ] && export LIBCLANG_PATH=$(ls -d /usr/lib/llvm-*/lib 2>/dev/null | sort -V | tail -1)
