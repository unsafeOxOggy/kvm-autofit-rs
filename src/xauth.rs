//! Minimal, bounded Xauthority reader.
//!
//! Format: repeated entries of `family: u16 BE` followed by four counted strings
//! (`len: u16 BE` + bytes): address, display number, auth name, auth data.
//! Only MIT-MAGIC-COOKIE-1 entries with a 16-byte cookie are accepted.

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use rustix::fs::{Mode, OFlags};

pub const MIT_MAGIC_COOKIE: &[u8] = b"MIT-MAGIC-COOKIE-1";
const COOKIE_LEN: usize = 16;
/// Real Xauthority files are a few hundred bytes; anything bigger is refused.
pub const MAX_FILE_SIZE: u64 = 64 * 1024;
const FAMILY_LOCAL: u16 = 256;
const FAMILY_WILD: u16 = 65535;

/// An X authorization cookie. `Debug` never prints the secret.
pub struct Cookie {
    pub name: Vec<u8>,
    pub data: Vec<u8>,
}

impl fmt::Debug for Cookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Cookie({}, <redacted>)",
            String::from_utf8_lossy(&self.name)
        )
    }
}

struct Entry<'a> {
    family: u16,
    address: &'a [u8],
    number: &'a [u8],
    name: &'a [u8],
    data: &'a [u8],
}

struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn u16(&mut self) -> Option<u16> {
        let (head, rest) = self.buf.split_first_chunk::<2>()?;
        self.buf = rest;
        Some(u16::from_be_bytes(*head))
    }

    fn counted(&mut self) -> Option<&'a [u8]> {
        let len = usize::from(self.u16()?);
        if len > self.buf.len() {
            return None;
        }
        let (head, rest) = self.buf.split_at(len);
        self.buf = rest;
        Some(head)
    }

    fn entry(&mut self) -> Option<Entry<'a>> {
        Some(Entry {
            family: self.u16()?,
            address: self.counted()?,
            number: self.counted()?,
            name: self.counted()?,
            data: self.counted()?,
        })
    }
}

/// Pick the best cookie for local display `display` on host `hostname`.
/// A truncated or malformed tail simply ends the scan.
pub fn select(buf: &[u8], display: u16, hostname: &[u8]) -> Option<Cookie> {
    let number = display.to_string();
    let mut reader = Reader { buf };
    let mut best: Option<(u8, Entry<'_>)> = None;
    while let Some(e) = reader.entry() {
        if e.name != MIT_MAGIC_COOKIE || e.data.len() != COOKIE_LEN {
            continue;
        }
        if !e.number.is_empty() && e.number != number.as_bytes() {
            continue;
        }
        let score = match e.family {
            FAMILY_LOCAL if e.address == hostname => 3,
            FAMILY_WILD => 2,
            FAMILY_LOCAL => 1, // hostname changed since login: still a local cookie
            _ => continue,
        };
        if best.as_ref().is_none_or(|(s, _)| score > *s) {
            best = Some((score, e));
        }
    }
    best.map(|(_, e)| Cookie {
        name: e.name.to_vec(),
        data: e.data.to_vec(),
    })
}

/// Read an Xauthority file safely on behalf of root.
///
/// The file must be an absolute path, not a symlink (final component), a regular
/// file, owned by one of `allowed_owners`, and at most `MAX_FILE_SIZE` bytes.
/// This prevents a user from making root read e.g. /etc/shadow through a forged
/// `-auth` argument or XAUTHORITY variable.
pub fn read_checked(path: &Path, allowed_owners: &[u32]) -> Result<Vec<u8>, String> {
    if !path.is_absolute() {
        return Err("auth path is not absolute".into());
    }
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOCTTY,
        Mode::empty(),
    )
    .map_err(|e| format!("open auth file: {e}"))?;
    let st = rustix::fs::fstat(&fd).map_err(|e| format!("stat auth file: {e}"))?;
    if rustix::fs::FileType::from_raw_mode(st.st_mode) != rustix::fs::FileType::RegularFile {
        return Err("auth file is not a regular file".into());
    }
    if !allowed_owners.contains(&st.st_uid) {
        return Err(format!(
            "auth file owned by uid {} (not trusted for this X server)",
            st.st_uid
        ));
    }
    let mut buf = Vec::new();
    File::from(fd)
        .take(MAX_FILE_SIZE + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("read auth file: {e}"))?;
    if buf.len() as u64 > MAX_FILE_SIZE {
        return Err("auth file too large".into());
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(family: u16, addr: &[u8], num: &[u8], name: &[u8], data: &[u8]) -> Vec<u8> {
        let mut v = family.to_be_bytes().to_vec();
        for s in [addr, num, name, data] {
            v.extend((s.len() as u16).to_be_bytes());
            v.extend(s);
        }
        v
    }

    #[test]
    fn prefers_local_hostname_match() {
        let mut f = entry(FAMILY_WILD, b"", b"0", MIT_MAGIC_COOKIE, &[1; 16]);
        f.extend(entry(
            FAMILY_LOCAL,
            b"kali",
            b"0",
            MIT_MAGIC_COOKIE,
            &[2; 16],
        ));
        let c = select(&f, 0, b"kali").unwrap();
        assert_eq!(c.data, vec![2; 16]);
    }

    #[test]
    fn filters_display_number_name_and_length() {
        let mut f = entry(FAMILY_LOCAL, b"kali", b"1", MIT_MAGIC_COOKIE, &[1; 16]);
        f.extend(entry(
            FAMILY_LOCAL,
            b"kali",
            b"0",
            b"XDM-AUTHORIZATION-1",
            &[1; 16],
        ));
        f.extend(entry(
            FAMILY_LOCAL,
            b"kali",
            b"0",
            MIT_MAGIC_COOKIE,
            &[1; 15],
        ));
        assert!(select(&f, 0, b"kali").is_none());
    }

    #[test]
    fn malformed_input_never_panics() {
        assert!(select(&[], 0, b"h").is_none());
        assert!(select(&[0xff], 0, b"h").is_none());
        assert!(select(&[1, 0, 0xff, 0xff, 1], 0, b"h").is_none());
        // valid entry followed by a truncated one
        let mut f = entry(FAMILY_LOCAL, b"h", b"0", MIT_MAGIC_COOKIE, &[7; 16]);
        f.extend([1, 0, 0, 9, b'x']);
        assert_eq!(select(&f, 0, b"h").unwrap().data, vec![7; 16]);
    }

    #[test]
    fn debug_redacts_secret() {
        let c = Cookie {
            name: MIT_MAGIC_COOKIE.to_vec(),
            data: vec![0xab; 16],
        };
        assert!(!format!("{c:?}").contains("171"));
        assert!(format!("{c:?}").contains("redacted"));
    }

    #[test]
    fn read_checked_rejects_symlink_relative_and_wrong_owner() {
        let dir = std::env::temp_dir().join(format!("kvm-autofit-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("auth");
        std::fs::write(&file, b"x").unwrap();
        let link = dir.join("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&file, &link).unwrap();
        let me = rustix::process::getuid().as_raw();

        assert!(read_checked(Path::new("relative"), &[me]).is_err());
        assert!(read_checked(&link, &[me]).is_err());
        assert!(read_checked(&file, &[me + 1]).is_err());
        assert!(read_checked(&dir, &[me]).is_err());
        assert_eq!(read_checked(&file, &[me]).unwrap(), b"x");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
