/*
 * kmsfix.c — let ffmpeg's kmsgrab capture KWin's 10-bit framebuffers.
 *
 * KWin 6 scans out ARGB2101010 ('AR30') / ABGR2101010 ('AB30') even with
 * KWIN_DRM_PREFER_COLOR_DEPTH=24. kmsgrab only knows the X (no-alpha)
 * variants, which have the identical memory layout (alpha bits ignored).
 * This library wraps drmModeGetFB2() and reports AR30/AB30 as XR30/XB30;
 * the frames then go hwmap → VAAPI (x2rgb10/x2bgr10) like any other.
 * No copies, no added latency: only the format label changes.
 *
 * ffmpeg-kms has file capabilities, so LD_PRELOAD is ignored; link it in.
 * The library must live in a root-owned directory: a capability binary that
 * loads a user-writable library hands CAP_SYS_ADMIN to anything running as you.
 *   gcc -shared -fPIC -O2 -I/usr/include/libdrm -o libkmsfix.so legacy/kmsfix.c -ldl
 *   sudo install -m 755 -o root -g root libkmsfix.so /usr/local/lib/libkmsfix.so
 *   cp /usr/bin/ffmpeg ~/.local/bin/ffmpeg-kms
 *   patchelf --add-needed /usr/local/lib/libkmsfix.so ~/.local/bin/ffmpeg-kms
 *   sudo setcap cap_sys_admin+ep ~/.local/bin/ffmpeg-kms     (after patching)
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdint.h>
#include <xf86drmMode.h>
#include <libdrm/drm_fourcc.h>

drmModeFB2Ptr drmModeGetFB2(int fd, uint32_t fb_id) {
    static drmModeFB2Ptr (*real)(int, uint32_t);
    if (!real) real = (drmModeFB2Ptr (*)(int, uint32_t))dlsym(RTLD_NEXT, "drmModeGetFB2");
    if (!real) return NULL;
    drmModeFB2Ptr fb = real(fd, fb_id);
    if (fb) {
        if (fb->pixel_format == DRM_FORMAT_ARGB2101010) fb->pixel_format = DRM_FORMAT_XRGB2101010;
        else if (fb->pixel_format == DRM_FORMAT_ABGR2101010) fb->pixel_format = DRM_FORMAT_XBGR2101010;
    }
    return fb;
}
