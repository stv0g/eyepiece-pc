#!/bin/bash
# eyepiece-vr.sh — run Monado (OpenXR) for the goggles' stereo mode. Nothing here is
# needed for normal (2D) eyepiece use; stop it with Ctrl+C when you are done with VR.
#
#   1. monado-service with its `remote` driver (head pose over TCP 4242) and the compositor
#      in an X11 window;
#   2. eyepiece-xr: goggles pose (eyepiece-pc's opentrack UDP output) → Monado;
#   3. a watcher that resizes Monado's window to 2560x720 (left | right eye) at X,Y as soon
#      as an OpenXR app starts (Monado creates the window with the first session).
#
# Then, in another terminal:
#   eyepiece-pc --stereo --source x11 --region 2560x720+X+Y --head-tracking --opentrack 127.0.0.1:4243   (X11 session)
#   eyepiece-pc --stereo --source pipewire --head-tracking --opentrack 127.0.0.1:4243        (Wayland: pick the
#       "Monado" window in the portal dialog; root-window X11 capture is black under Xwayland)
# and start an OpenXR app (e.g. `hello_xr -g Vulkan2`).
#
# Environment: EYEPIECE_XR (path of eyepiece-xr), XR_POS (window position, default 0,0),
# XR_UDP (pose input, default 127.0.0.1:4243), XR_ARGS (extra eyepiece-xr options, e.g.
# "--ipd 65 --predict 30"). Needs: monado-service, xdotool, xwininfo (x11-utils).
set -u
XR=${EYEPIECE_XR:-$(command -v eyepiece-xr || echo "$(dirname "$0")/../../target/release/eyepiece-xr")}
POS=${XR_POS:-0,0}
UDP=${XR_UDP:-127.0.0.1:4243}
for t in monado-service xdotool xwininfo; do
    command -v $t >/dev/null || { echo "eyepiece-vr: $t not found" >&2; exit 1; }
done
[ -x "$XR" ] || { echo "eyepiece-vr: eyepiece-xr not found (build: cargo build --release -p eyepiece-xr)" >&2; exit 1; }

pids=()
cleanup() {
    trap - EXIT INT TERM
    kill "${pids[@]}" 2>/dev/null
    for _ in 1 2 3 4 5 6; do kill -0 "${pids[@]}" 2>/dev/null || break; sleep 0.5; done
    kill -9 "${pids[@]}" 2>/dev/null
    rm -rf "$tmp"
    echo "eyepiece-vr: stopped"
    exit 0
}
trap cleanup EXIT INT TERM

# monado-service quits on Enter / EOF on stdin, so give it a stdin that stays open:
# a FIFO this script holds open itself (no helper process to leak).
# P_OVERRIDE_ACTIVE_CONFIG=remote selects the remote driver without a config file.
tmp=$(mktemp -d); mkfifo "$tmp/stdin"; exec 3<>"$tmp/stdin"
P_OVERRIDE_ACTIVE_CONFIG=remote XRT_COMPOSITOR_FORCE_XCB=1 monado-service <&3 &
pids+=($!)
"$XR" --listen "$UDP" ${XR_ARGS:-} &
pids+=($!)

echo "eyepiece-vr: waiting for an OpenXR app (Monado opens its window then)"
X=${POS%,*}; Y=${POS#*,}; done_w=
while :; do
    W=$(xwininfo -root -tree 2>/dev/null | awk '/"Monado": /{print $1; exit}')
    if [ -n "$W" ] && [ "$W" != "$done_w" ]; then
        sleep 1   # let the compositor finish creating it
        xdotool windowsize "$W" 2560 720 windowmove "$W" "$X" "$Y"
        sleep 1
        G=$(xwininfo -id "$W" | awk '/Width:/{w=$2} /Height:/{h=$2} /Absolute upper-left X:/{x=$4} /Absolute upper-left Y:/{y=$4} END{print w"x"h"+"x"+"y}')
        echo "eyepiece-vr: Monado window $W is now $G → eyepiece-pc --stereo --source x11 --region $G"
        done_w=$W
    fi
    sleep 1
done
