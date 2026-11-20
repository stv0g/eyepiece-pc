//! `adb` (platform-tools) helpers: forward, deploy, stop — same behaviour as vrstream.py.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

pub const DEVICE_DIR: &str = "/data/local/tmp/usbvr";

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
        if let Ok(pid) = self.shell("pidof vr_bridge", false) {
            if !pid.is_empty() {
                let _ = self.shell(&format!("kill {pid}"), false);
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }

    /// Push `bridge` to the goggles and start it with `-1` (exit with the PC session: a
    /// bridge left running keeps the goggles black) plus `extra_args`.
    pub fn deploy(&self, bridge: &Path, extra_args: &str, log: &dyn Fn(String)) -> Result<(), String> {
        if !bridge.is_file() {
            return Err(format!("{} not found — build it with usb-vr/device/build.sh", bridge.display()));
        }
        self.shell(&format!("mkdir -p {DEVICE_DIR}"), true)?;
        self.run(&["push", &bridge.to_string_lossy(), &format!("{DEVICE_DIR}/vr_bridge")], true)?;
        self.shell(&format!("chmod 755 {DEVICE_DIR}/vr_bridge"), true)?;
        self.stop_bridge();
        self.shell(
            &format!("nohup {DEVICE_DIR}/vr_bridge -1 {extra_args} >/dev/null 2>&1 < /dev/null &"),
            true,
        )?;
        std::thread::sleep(Duration::from_millis(800));
        let pid = self.shell("pidof vr_bridge", false).unwrap_or_default();
        if pid.is_empty() {
            let tail = self.shell("tail -n 20 /data/local/tmp/usbvr.log", false).unwrap_or_default();
            return Err(format!("vr_bridge did not start:\n{tail}"));
        }
        log(format!("vr_bridge running on goggles (pid {pid})"));
        let nat = self
            .shell("grep -c 'hook_overlay loaded' /data/local/tmp/natartos.log", false)
            .unwrap_or_default();
        if nat.trim().is_empty() || nat.trim() == "0" {
            log("WARNING: natartos overlay not seen in natartos.log — video needs `toolya natartos start` \
                 (overlay mod injects fb_shared into the display)"
                .into());
        }
        Ok(())
    }
}
