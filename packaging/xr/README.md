# Eyepiece VR (Linux, Monado) — experimental

Uses the goggles' stereo mode (`eyepiece-pc --stereo`: one 2560×720 side-by-side stream,
1280×720 per eye) as an OpenXR headset through [Monado](https://monado.freedesktop.org/).
Nothing here is linked into `eyepiece-pc`; Monado only runs while you use VR.

```
OpenXR app ─► monado-service (remote driver, X11 window 2560×720 = left | right eye)
                 ▲ head pose (TCP 4242)                 │ window capture
             eyepiece-xr ◄── opentrack UDP 4243 ── eyepiece-pc --stereo ──► goggles
```

## Install

```sh
sudo apt install monado-service libopenxr1-monado libopenxr-utils xdotool x11-utils   # Debian/Ubuntu
cargo build --release -p eyepiece-xr
```

## Run

```sh
packaging/xr/eyepiece-vr.sh            # monado-service + eyepiece-xr + window placement; Ctrl+C stops all
hello_xr -g Vulkan2                    # or any OpenXR app; Monado opens its window now
# X11 session:
eyepiece-pc --stereo --source x11 --region 2560x720+X+Y --head-tracking --opentrack 127.0.0.1:4243
# Wayland session (root-window capture of Xwayland is black): pick the "Monado" window
eyepiece-pc --stereo --source pipewire --head-tracking --opentrack 127.0.0.1:4243
```

`eyepiece-vr.sh` prints the window geometry once an app has started. Options for the pose
bridge go into `XR_ARGS`, e.g. `XR_ARGS="--ipd 65 --predict 30"`; see `eyepiece-xr --help`.
`eyepiece-xr --test` sends a synthetic head sweep (no goggles needed).

## Known limits (Monado 25.0)

- **FOV:** the remote driver's display is fixed at 85° per eye. eyepiece-xr sends the
  goggles' FOV (≈38.8°×22.4°, from the 44° diagonal spec) as per-view data, so apps render
  the right projection, but Monado composites it into its 85° view: the picture is small in
  the middle of each eye. `--driver-fov` makes it fill the window with the wrong scale (for
  testing the pipeline). The real fix is a Monado driver with configurable FOV.
- **No pose prediction** in the remote driver (it always uses the latest pose); eyepiece-xr
  `--predict MS` extrapolates instead.
- The remote driver listens on **all interfaces** (TCP 4242); firewall it on untrusted networks.
- Controllers are reported inactive.
