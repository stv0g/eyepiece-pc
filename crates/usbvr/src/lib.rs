//! usb-vr PC streaming library: everything a frontend (the `usbvr` CLI, later a UI)
//! needs. Build a [`session::Config`], call [`session::run`] on a thread, steer it with
//! [`session::Control`], receive log lines and [`usbvr_link::stats::Snapshot`]s via
//! [`session::Events`].

pub mod preview;
pub mod session;

pub use session::{run, Config, Control, Events, SourceKind};
pub use usbvr_av as av;
pub use usbvr_capture as capture;
pub use usbvr_link as link;
