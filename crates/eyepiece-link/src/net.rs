//! The goggles' USB network (RNDIS): a lower-latency path than `adb forward`.
//!
//! The bridge switches the goggles' USB to RNDIS + adb once per boot and serves DHCP on it
//! (192.168.60.2 = goggles, 192.168.60.1 = PC; no router, no DNS). Bringing the PC side up
//! is left to the user's network setup (NetworkManager, systemd-networkd, …): on Linux we
//! find the interface and check whether it has an address; on every OS
//! [`goggles_reachable`] checks that the goggles answer on it.

/// The goggles' address on their USB network.
pub const GOGGLES_ADDR: &str = "192.168.60.2";

/// DJI's USB vendor id.
#[cfg(target_os = "linux")]
const DJI_VID: &str = "2ca3";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsbNet {
    /// PC interface name, e.g. `enx…`
    pub ifname: String,
    /// the interface has an address in 192.168.60.0/24 (DHCP from the goggles done)
    pub configured: bool,
}

/// Find the goggles' RNDIS interface on this PC (Linux; `None` elsewhere / not present).
#[cfg(target_os = "linux")]
pub fn find_usb_net() -> Option<UsbNet> {
    use std::fs;
    for e in fs::read_dir("/sys/class/net").ok()?.flatten() {
        let ifname = e.file_name().to_string_lossy().to_string();
        let dev = e.path().join("device");
        // device → USB interface (…/3-1.2:1.0); its parent is the USB device
        let Ok(intf) = fs::canonicalize(&dev) else { continue };
        let driver = fs::read_link(intf.join("driver"))
            .ok()
            .and_then(|d| d.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_default();
        if driver != "rndis_host" {
            continue;
        }
        let Some(usb) = intf.parent() else { continue };
        let vid = fs::read_to_string(usb.join("idVendor")).unwrap_or_default();
        if vid.trim() != DJI_VID {
            continue;
        }
        let configured = has_goggles_subnet_addr(&ifname);
        return Some(UsbNet { ifname, configured });
    }
    None
}

#[cfg(not(target_os = "linux"))]
pub fn find_usb_net() -> Option<UsbNet> {
    None
}

/// Does `ifname` have an IPv4 address in 192.168.60.0/24?
#[cfg(target_os = "linux")]
fn has_goggles_subnet_addr(ifname: &str) -> bool {
    let mut found = false;
    unsafe {
        let mut ifa: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifa) != 0 {
            return false;
        }
        let mut p = ifa;
        while !p.is_null() {
            let a = &*p;
            if !a.ifa_addr.is_null()
                && (*a.ifa_addr).sa_family as i32 == libc::AF_INET
                && std::ffi::CStr::from_ptr(a.ifa_name).to_bytes() == ifname.as_bytes()
            {
                let sin = &*(a.ifa_addr as *const libc::sockaddr_in);
                let ip = u32::from_be(sin.sin_addr.s_addr);
                if ip >> 8 == (192 << 16 | 168 << 8 | 60) {
                    found = true;
                }
            }
            p = a.ifa_next;
        }
        libc::freeifaddrs(ifa);
    }
    found
}

/// Wait up to `timeout` for the interface to get its address (DHCP after the USB switch).
pub fn wait_usb_net(timeout: std::time::Duration) -> Option<UsbNet> {
    let t0 = std::time::Instant::now();
    loop {
        let n = find_usb_net();
        if matches!(n, Some(UsbNet { configured: true, .. })) || t0.elapsed() >= timeout {
            return n;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// Port for [`goggles_reachable`]: nothing listens on it on the goggles (TCP discard).
const PROBE_PORT: u16 = 9;

/// Do the goggles answer on their USB network? A TCP connect to a port nothing listens on:
/// a refusal (RST) proves the goggles' network stack answered. Never the bridge port: a
/// session bridge (`-1`) exits after its first client disconnects. OS-independent (no
/// interface lookup), so it works where [`find_usb_net`] cannot look (Windows, macOS).
pub fn goggles_reachable(timeout: std::time::Duration) -> bool {
    use std::net::{SocketAddr, TcpStream};
    let addr: SocketAddr = format!("{GOGGLES_ADDR}:{PROBE_PORT}").parse().expect("static address");
    match TcpStream::connect_timeout(&addr, timeout) {
        Ok(_) => true, // something listens there after all: reachable too
        Err(e) => e.kind() == std::io::ErrorKind::ConnectionRefused,
    }
}

/// Retry [`goggles_reachable`] for up to `timeout` (the PC side needs a moment after the
/// bridge's USB switch: driver bind + DHCP).
pub fn wait_goggles_reachable(timeout: std::time::Duration) -> bool {
    let t0 = std::time::Instant::now();
    loop {
        // Windows answers a refused connect only after its SYN retries (~1 s)
        if goggles_reachable(std::time::Duration::from_millis(1500)) {
            return true;
        }
        if t0.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}
