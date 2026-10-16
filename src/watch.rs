//! Daemon: listen to kernel uevents (netlink, no libudev) and refit on DRM changes.
//!
//! Only messages sent by the kernel (netlink port id 0) are accepted; each message
//! is size-bounded and strictly parsed. Bursts (window being dragged) are coalesced,
//! and refits are rate-limited so a flood of events cannot pin the CPU.

use std::thread::sleep;
use std::time::{Duration, Instant};

use rustix::fd::OwnedFd;
use rustix::net::netlink::{self, SocketAddrNetlink};
use rustix::net::{AddressFamily, RecvFlags, SocketType};

use crate::info;

const KERNEL_GROUP: u32 = 1;
const BUF_SIZE: usize = 8192;
const COALESCE: Duration = Duration::from_millis(150);
const MIN_INTERVAL: Duration = Duration::from_millis(300);

/// True for a kernel uevent "change" on a DRM card (resize hint / hotplug).
pub fn is_drm_change(msg: &[u8]) -> bool {
    let mut fields = msg.split(|&b| b == 0);
    // Kernel messages start with "ACTION@DEVPATH"; libudev ones with "libudev".
    let Some(header) = fields.next() else {
        return false;
    };
    if !header.starts_with(b"change@") {
        return false;
    }
    let (mut action, mut subsystem, mut devname_ok) = (false, false, true);
    for f in fields {
        match f {
            b"ACTION=change" => action = true,
            b"SUBSYSTEM=drm" => subsystem = true,
            _ => {
                if let Some(name) = f.strip_prefix(b"DEVNAME=") {
                    // dri/card0 is the KMS device; renderD* nodes never carry mode hints.
                    devname_ok = name.starts_with(b"dri/card");
                }
            }
        }
    }
    action && subsystem && devname_ok
}

fn open_socket() -> std::io::Result<OwnedFd> {
    let fd = rustix::net::socket(
        AddressFamily::NETLINK,
        SocketType::DGRAM,
        Some(netlink::KOBJECT_UEVENT),
    )?;
    rustix::net::bind(&fd, &SocketAddrNetlink::new(0, KERNEL_GROUP))?;
    Ok(fd)
}

/// Receive one message; Ok(None) for anything not from the kernel, truncated, or not DRM.
fn recv_drm(fd: &OwnedFd, buf: &mut [u8], flags: RecvFlags) -> std::io::Result<Option<()>> {
    let (len, full_len, addr) = rustix::net::recvfrom(fd, &mut *buf, flags)?;
    if full_len > len {
        return Ok(None); // truncated: ignore rather than parse a partial message
    }
    let from_kernel = addr
        .and_then(|a| SocketAddrNetlink::try_from(a).ok())
        .is_some_and(|a| a.pid() == 0);
    Ok((from_kernel && buf.get(..len).is_some_and(is_drm_change)).then_some(()))
}

/// Never returns unless the netlink socket cannot be opened.
pub fn run(mut refit: impl FnMut()) -> std::io::Result<()> {
    let fd = open_socket()?;
    info!("watching DRM uevents");
    refit(); // initial fit: the daemon may start after the viewer already resized
    let mut buf = vec![0u8; BUF_SIZE];
    let mut last = Instant::now() - MIN_INTERVAL;
    loop {
        match recv_drm(&fd, &mut buf, RecvFlags::empty()) {
            Ok(Some(())) => {}
            Ok(None) => continue,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                // ENOBUFS (event storm) and friends: log, back off, keep running.
                info!("uevent receive error: {err}");
                sleep(Duration::from_secs(1));
                continue;
            }
        }
        // Coalesce the burst, then drain whatever queued meanwhile.
        sleep(COALESCE);
        while let Ok(Some(_) | None) = recv_drm(&fd, &mut buf, RecvFlags::DONTWAIT) {}
        let since = last.elapsed();
        if since < MIN_INTERVAL {
            sleep(MIN_INTERVAL - since);
        }
        refit();
        last = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::is_drm_change;

    #[test]
    fn accepts_kernel_drm_change() {
        let m = b"change@/devices/pci0000:00/0000:00:01.0/virtio0/drm/card0\0ACTION=change\0DEVPATH=/devices/pci0000:00/0000:00:01.0/virtio0/drm/card0\0SUBSYSTEM=drm\0HOTPLUG=1\0DEVNAME=dri/card0\0SEQNUM=4242\0";
        assert!(is_drm_change(m));
    }

    #[test]
    fn rejects_other_events() {
        assert!(!is_drm_change(b""));
        assert!(!is_drm_change(b"\0\0\0"));
        assert!(!is_drm_change(b"add@/x\0ACTION=add\0SUBSYSTEM=drm\0"));
        assert!(!is_drm_change(
            b"change@/x\0ACTION=change\0SUBSYSTEM=block\0"
        ));
        assert!(!is_drm_change(b"libudev\0ACTION=change\0SUBSYSTEM=drm\0"));
        assert!(!is_drm_change(
            b"change@/x\0ACTION=change\0SUBSYSTEM=drm\0DEVNAME=dri/renderD128\0"
        ));
        // header alone is not enough
        assert!(!is_drm_change(b"change@/devices/drm/card0\0"));
    }
}
