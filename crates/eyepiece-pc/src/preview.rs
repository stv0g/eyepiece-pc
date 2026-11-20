//! Preview: the exact encoded stream (what the goggles get, artifacts included) decoded
//! by ffplay. Fed from a bounded channel: if ffplay falls behind it loses data, never the
//! goggles.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{sync_channel, SyncSender};
use eyepiece_av::Codec;
use eyepiece_link::LogFn;

pub struct Preview {
    tx: Option<SyncSender<Vec<u8>>>,
    child: Child,
}

impl Preview {
    pub fn spawn(pos: (i32, i32), codec: Codec, fps: u32, log: &LogFn) -> Result<Preview, String> {
        let mut child = Command::new("ffplay")
            .args(["-hide_banner", "-loglevel", "error", "-fflags", "nobuffer", "-flags", "low_delay", "-framedrop"])
            .args(["-probesize", "32", "-analyzeduration", "0", "-sync", "ext"])
            .args(["-window_title", "eyepiece-pc preview (what the goggles get)"])
            .args(["-left", &pos.0.to_string(), "-top", &pos.1.to_string(), "-x", "1920", "-y", "1080"])
            .args(["-framerate", &fps.to_string()])
            .args(["-f", if codec == Codec::Hevc { "hevc" } else { "h264" }, "-i", "-"])
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| format!("--preview needs ffplay: {e}"))?;
        let mut stdin = child.stdin.take().unwrap();
        let (tx, rx) = sync_channel::<Vec<u8>>(16);
        std::thread::Builder::new()
            .name("ep-preview".into())
            .spawn(move || {
                while let Ok(b) = rx.recv() {
                    if stdin.write_all(&b).is_err() {
                        return;
                    }
                }
            })
            .unwrap();
        log("preview: ffplay window".into());
        Ok(Preview { tx: Some(tx), child })
    }

    pub fn feed(&mut self, au: &[u8]) {
        if let Some(tx) = &self.tx {
            let _ = tx.try_send(au.to_vec());
        }
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.tx = None;
        let _ = self.child.kill(); // ffplay ignores SIGTERM while its SDL window is open
        let _ = self.child.wait();
    }
}
