//! CAP_SYS_ADMIN (file capability for KMS capture, see README) in the GUI process.
//!
//! Capabilities are per thread on Linux. At startup the UI thread clears its *effective* set
//! (keeping *permitted*), so the window, GL driver and preview threads run without it. A KMS
//! session thread raises it again before `cli::prepare`, whose `limit_privileges` then works
//! as in the CLI (non-KMS sessions drop everything on their thread).

#[cfg(target_os = "linux")]
mod imp {
    #[repr(C)]
    struct Hdr {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    const V3: u32 = 0x2008_0522;

    fn get() -> Option<[Data; 2]> {
        let mut h = Hdr { version: V3, pid: 0 };
        let mut d = [Data::default(); 2];
        let r = unsafe { libc::syscall(libc::SYS_capget, &mut h as *mut Hdr, d.as_mut_ptr()) };
        (r == 0).then_some(d)
    }

    fn set(d: &[Data; 2]) -> bool {
        let mut h = Hdr { version: V3, pid: 0 };
        unsafe { libc::syscall(libc::SYS_capset, &mut h as *mut Hdr, d.as_ptr()) == 0 }
    }

    /// Calling thread: effective := empty, permitted kept. No-op without capabilities.
    pub fn lower_effective() {
        if let Some(mut d) = get() {
            if d.iter().any(|x| x.effective != 0) {
                d.iter_mut().for_each(|x| x.effective = 0);
                set(&d);
            }
        }
    }

    /// KMS capture can work: root, or CAP_SYS_ADMIN (bit 21) permitted.
    pub fn kms_allowed() -> bool {
        let root = unsafe { libc::geteuid() } == 0;
        root || get().is_some_and(|d| d[0].permitted & (1 << 21) != 0)
    }

    /// Calling thread: effective := permitted.
    pub fn raise_effective() {
        if let Some(mut d) = get() {
            if d.iter().any(|x| x.permitted != x.effective) {
                d.iter_mut().for_each(|x| x.effective = x.permitted);
                set(&d);
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    pub fn lower_effective() {}
    pub fn raise_effective() {}
    pub fn kms_allowed() -> bool {
        false
    }
}

pub use imp::{kms_allowed, lower_effective, raise_effective};
