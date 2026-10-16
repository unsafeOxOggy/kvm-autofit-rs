//! Discover running Xorg servers and open authenticated connections to them.
//!
//! Everything read from /proc is attacker-influenced (any user can start a process
//! named `Xorg` with arbitrary arguments, or set XAUTHORITY to any path), so:
//! - display numbers are strictly parsed,
//! - an auth file is only read if owned by the X server's uid (see `read_checked`),
//! - the socket peer must be that same uid (SO_PEERCRED) before a cookie is sent.

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use x11rb::rust_connection::{DefaultStream, RustConnection};

use crate::xauth::{self, Cookie};

const MAX_PROC_READ: u64 = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XServer {
    pub display: u16,
    pub pid: u32,
    pub uid: u32,
    pub auth: Option<PathBuf>,
}

/// Parse ":N" or ":N.S" (local displays only). Rejects hosts, signs, overlong input.
pub fn parse_display(s: &str) -> Option<u16> {
    let rest = s.strip_prefix(':')?;
    let num = rest.split_once('.').map_or(rest, |(n, screen)| {
        if screen.is_empty() || screen.len() > 3 || !screen.bytes().all(|b| b.is_ascii_digit()) {
            ""
        } else {
            n
        }
    });
    if num.is_empty() || num.len() > 5 || !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    num.parse().ok()
}

/// Extract the display and `-auth` file from an Xorg argv.
pub fn parse_xorg_args(argv: &[&[u8]]) -> (Option<u16>, Option<PathBuf>) {
    let mut display = None;
    let mut auth = None;
    let mut it = argv.iter().skip(1);
    while let Some(arg) = it.next() {
        if *arg == b"-auth" {
            auth = it
                .next()
                .and_then(|p| std::str::from_utf8(p).ok())
                .map(PathBuf::from)
                .filter(|p| p.is_absolute());
        } else if let Some(d) = std::str::from_utf8(arg).ok().and_then(parse_display) {
            display = Some(d);
        }
    }
    (display, auth)
}

fn read_proc(path: &Path) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    File::open(path)
        .ok()?
        .take(MAX_PROC_READ)
        .read_to_end(&mut buf)
        .ok()?;
    Some(buf)
}

fn pids() -> impl Iterator<Item = u32> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
}

/// All running Xorg servers, sorted by display, one per display.
pub fn discover() -> Vec<XServer> {
    let mut out: Vec<XServer> = Vec::new();
    for pid in pids() {
        let base = PathBuf::from(format!("/proc/{pid}"));
        let Some(comm) = read_proc(&base.join("comm")) else {
            continue;
        };
        if !matches!(comm.trim_ascii_end(), b"Xorg" | b"X") {
            continue;
        }
        let Ok(uid) = std::fs::metadata(&base).map(|m| m.uid()) else {
            continue;
        };
        let Some(cmdline) = read_proc(&base.join("cmdline")) else {
            continue;
        };
        let argv: Vec<&[u8]> = cmdline
            .split(|&b| b == 0)
            .filter(|a| !a.is_empty())
            .collect();
        if let (Some(display), auth) = parse_xorg_args(&argv) {
            out.push(XServer {
                display,
                pid,
                uid,
                auth,
            });
        }
    }
    out.sort_by_key(|s| s.display);
    out.dedup_by_key(|s| s.display);
    out
}

/// Fallback when Xorg has no `-auth`: the XAUTHORITY of a client process of that display.
/// Returns the path and the uid of the client that uses it.
fn client_auth(display: u16) -> Option<(PathBuf, u32)> {
    for pid in pids() {
        let base = PathBuf::from(format!("/proc/{pid}"));
        let Some(env) = read_proc(&base.join("environ")) else {
            continue;
        };
        let mut disp = None;
        let mut auth = None;
        for var in env.split(|&b| b == 0) {
            if let Some(v) = var.strip_prefix(b"DISPLAY=") {
                disp = std::str::from_utf8(v).ok().and_then(parse_display);
            } else if let Some(v) = var.strip_prefix(b"XAUTHORITY=") {
                auth = std::str::from_utf8(v)
                    .ok()
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute());
            }
        }
        if let (Some(d), Some(a)) = (disp, auth)
            && d == display
            && let Ok(uid) = std::fs::metadata(&base).map(|m| m.uid())
        {
            return Some((a, uid));
        }
    }
    None
}

fn hostname() -> Vec<u8> {
    rustix::system::uname().nodename().to_bytes().to_vec()
}

/// Find a cookie for `srv`, enforcing ownership rules:
/// - `-auth` file: must belong to the X server's uid;
/// - client XAUTHORITY fallback: must belong to that client, and only for a root
///   X server (a non-root server only ever receives files of its own uid).
pub fn cookie_for(srv: &XServer) -> Result<Option<Cookie>, String> {
    let (path, owners) = match &srv.auth {
        Some(p) => (p.clone(), vec![srv.uid]),
        None => match client_auth(srv.display) {
            Some((p, client_uid)) if srv.uid == 0 || client_uid == srv.uid => (p, vec![client_uid]),
            Some(_) => return Err("client auth file not trusted for a non-root X server".into()),
            None => return Ok(None),
        },
    };
    let buf = xauth::read_checked(&path, &owners)?;
    Ok(xauth::select(&buf, srv.display, &hostname()))
}

/// Connect to a discovered server: verify the socket peer uid, then authenticate.
pub fn connect_server(srv: &XServer) -> Result<RustConnection, String> {
    let cookie = cookie_for(srv)?;
    let path = format!("/tmp/.X11-unix/X{}", srv.display);
    let stream = UnixStream::connect(&path).map_err(|e| format!("connect {path}: {e}"))?;
    let peer =
        rustix::net::sockopt::socket_peercred(&stream).map_err(|e| format!("peercred: {e}"))?;
    if peer.uid.as_raw() != srv.uid {
        return Err(format!(
            "socket {path} is served by uid {}, expected {}",
            peer.uid.as_raw(),
            srv.uid
        ));
    }
    let (stream, _) =
        DefaultStream::from_unix_stream(stream).map_err(|e| format!("stream: {e}"))?;
    let (name, data) = cookie.map(|c| (c.name, c.data)).unwrap_or_default();
    RustConnection::connect_to_stream_with_auth_info(stream, 0, name, data)
        .map_err(|e| format!("X handshake on :{}: {e}", srv.display))
}

/// Connect as the current user to a local display (standard XAUTHORITY lookup).
pub fn connect_user(display: u16) -> Result<RustConnection, String> {
    x11rb::rust_connection::RustConnection::connect(Some(&format!(":{display}")))
        .map(|(c, _)| c)
        .map_err(|e| format!("connect :{display}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_parsing_is_strict() {
        assert_eq!(parse_display(":0"), Some(0));
        assert_eq!(parse_display(":12.0"), Some(12));
        for bad in [
            "0", ":", ":-1", ":+1", ":1.", ":1.x", "host:0", ":0 ", ":999999", ":1;id", ":٣",
        ] {
            assert_eq!(parse_display(bad), None, "{bad}");
        }
    }

    #[test]
    fn xorg_args() {
        let argv: Vec<&[u8]> = vec![
            b"/usr/lib/xorg/Xorg",
            b":0",
            b"-seat",
            b"seat0",
            b"-auth",
            b"/var/run/lightdm/root/:0",
            b"vt7",
        ];
        assert_eq!(
            parse_xorg_args(&argv),
            (Some(0), Some(PathBuf::from("/var/run/lightdm/root/:0")))
        );
        let rel: Vec<&[u8]> = vec![b"Xorg", b":1", b"-auth", b"relative"];
        assert_eq!(parse_xorg_args(&rel), (Some(1), None));
        let dangling: Vec<&[u8]> = vec![b"Xorg", b"-auth"];
        assert_eq!(parse_xorg_args(&dangling), (None, None));
        // the program name itself is never taken as a display
        let weird: Vec<&[u8]> = vec![b":7"];
        assert_eq!(parse_xorg_args(&weird), (None, None));
    }
}
