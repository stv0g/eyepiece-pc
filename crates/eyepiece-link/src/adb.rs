//! `adb` (platform-tools) helpers: forward, deploy, stop — same behaviour as eyepiece_stream.py.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Where `--deploy` pushes the bridge.
pub const DEVICE_DIR: &str = "/data/local/tmp/eyepiece";
/// The bridge installed by the natartos Eyepiece mod (started from the goggles' menu).
pub const MENU_BRIDGE: &str = "/vendor/natartos/apps/eyepiece/eyepiece_bridge";
/// The bridge's log on the goggles.
pub const DEVICE_LOG: &str = "/data/local/tmp/eyepiece.log";

#[derive(Clone, Debug)]
pub struct Adb {
    pub exe: String,
    pub serial: Option<String>,
}

impl Adb {
    pub fn new(exe: impl Into<String>, serial: Option<String>) -> Adb {
        Adb { exe: exe.into(), serial }
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(&self.exe);
        if let Some(s) = &self.serial {
            c.arg("-s").arg(s);
        }
        c
    }

    /// Run `adb <args>`; Err with stderr on failure (when `check`).
    pub fn run(&self, args: &[&str], check: bool) -> Result<String, String> {
        let out = self
            .cmd()
            .args(args)
            .output()
            .map_err(|e| format!("cannot run {}: {e} (is adb installed? --adb PATH)", self.exe))?;
        let so = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if check && !out.status.success() {
            let se = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(format!("adb {} failed: {}", args.join(" "), if se.is_empty() { so } else { se }));
        }
        Ok(so)
    }

    pub fn shell(&self, cmd: &str, check: bool) -> Result<String, String> {
        self.run(&["shell", cmd], check)
    }

    pub fn forward(&self, local: u16, remote: u16) -> Result<(), String> {
        self.run(&["forward", &format!("tcp:{local}"), &format!("tcp:{remote}")], true).map(|_| ())
    }

    pub fn stop_bridge(&self) {
        if let Ok(pid) = self.shell("pidof eyepiece_bridge", false) {
            if !pid.is_empty() {
                let _ = self.shell(&format!("kill {pid}"), false);
                // the bridge's helper sends the MCU head-attitude stop on SIGTERM
                std::thread::sleep(Duration::from_millis(2500));
            }
        }
        self.mcu_stop_safety();
    }

    /// Belt and braces for the MCU head-attitude stream: with no bridge running, send the
    /// stop (`eyepiece_bridge -Z`). Covers the case the bridge can't handle itself (bridge and
    /// helper both SIGKILLed): the stream outlives them and logs a router error per packet
    /// on the goggles. Harmless when the stream is already off. Returns the bridge's line.
    pub fn mcu_stop_safety(&self) -> Option<String> {
        if self.bridge_running() {
            return None;
        }
        let out = self
            .shell(
                &format!(
                    "for b in {DEVICE_DIR}/eyepiece_bridge {MENU_BRIDGE}; do \
                     [ -x $b ] && {{ $b -Z 2>&1 | grep 'MCU attitude'; break; }}; done"
                ),
                false,
            )
            .unwrap_or_default();
        let out = out.trim();
        // drop the bridge's own "[…] " log prefix
        let out = match out.strip_prefix('[').and_then(|r| r.split_once("] ")) {
            Some((_, rest)) => rest,
            None => out,
        };
        (!out.is_empty()).then(|| format!("goggles: {out}"))
    }

    /// Is the natartos overlay hook loaded into dji_glasses? Checks the process maps first
    /// (reliable), then natartos.log (may be truncated). True only on positive evidence.
    pub fn overlay_loaded(&self) -> bool {
        let out = self
            .shell(
                "for p in $(pidof dji_glasses); do grep -q hook_overlay /proc/$p/maps 2>/dev/null && \
                 { echo maps; exit 0; }; done; \
                 grep -q 'hook_overlay loaded' /data/local/tmp/natartos.log 2>/dev/null && echo log",
                false,
            )
            .unwrap_or_default();
        !out.trim().is_empty()
    }

    /// True if an eyepiece_bridge process runs on the goggles.
    pub fn bridge_running(&self) -> bool {
        !self.shell("pidof eyepiece_bridge", false).unwrap_or_default().trim().is_empty()
    }

    /// Push `bridge` to the goggles and start it with `-1` (exit with the PC session: a
    /// bridge left running keeps the goggles black) plus `extra_args`.
    pub fn deploy(&self, bridge: &Path, extra_args: &str, log: &dyn Fn(String)) -> Result<(), String> {
        if !bridge.is_file() {
            return Err(format!(
                "{} not found — download eyepiece_bridge from the natartos release or pass --bridge",
                bridge.display()
            ));
        }
        self.shell(&format!("mkdir -p {DEVICE_DIR}"), true)?;
        self.run(&["push", &bridge.to_string_lossy(), &format!("{DEVICE_DIR}/eyepiece_bridge")], true)?;
        self.shell(&format!("chmod 755 {DEVICE_DIR}/eyepiece_bridge"), true)?;
        self.stop_bridge();
        self.shell(
            &format!("nohup {DEVICE_DIR}/eyepiece_bridge -1 -b {extra_args} >/dev/null 2>&1 < /dev/null &"),
            true,
        )?;
        std::thread::sleep(Duration::from_millis(800));
        let pid = self.shell("pidof eyepiece_bridge", false).unwrap_or_default();
        if pid.is_empty() {
            let tail = self.shell(&format!("tail -n 20 {DEVICE_LOG}"), false).unwrap_or_default();
            return Err(format!("eyepiece_bridge did not start:\n{tail}"));
        }
        // adb forward accepts a TCP connect even when nothing listens on the goggles yet (and
        // closes it right away), so a connect is no proof: wait for the bridge's own log line.
        // Startup takes >1 s when the overlay has no fb_gui_hide (1 s wait for its ack).
        let t_end = std::time::Instant::now() + Duration::from_secs(6);
        while self.shell(&format!("grep -q 'listening on' {DEVICE_LOG} && echo y"), false).unwrap_or_default().trim() != "y" {
            if std::time::Instant::now() >= t_end {
                let tail = self.shell(&format!("tail -n 20 {DEVICE_LOG}"), false).unwrap_or_default();
                return Err(format!("eyepiece_bridge is not listening:\n{tail}"));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        log(format!("eyepiece_bridge running on goggles (pid {pid})"));
        if !self.overlay_loaded() {
            log("WARNING: natartos overlay not detected (hook_overlay.so is not mapped in dji_glasses) — \
                 if the goggles stay black, start natartos (`toolya natartos start`); its overlay mod \
                 puts the stream on the display"
                .into());
        }
        Ok(())
    }
}
