//! `kvm-autofit doctor`: read-only diagnosis of the guest display stack.

use std::path::Path;

use x11rb::connection::Connection;
use x11rb::protocol::randr::{self, ConnectionExt as _};

use crate::log::clean;
use crate::xserver;

const OK: &str = "[ok]";
const KO: &str = "[!!]";
const NA: &str = "[--]";

fn read(path: &str) -> String {
    std::fs::read_to_string(path)
        .map(|s| clean(s.trim()))
        .unwrap_or_default()
}

fn virtualization() -> String {
    // DMI vendor/product are enough to recognise QEMU/KVM without spawning tools.
    let vendor = read("/sys/class/dmi/id/sys_vendor");
    let product = read("/sys/class/dmi/id/product_name");
    format!("{vendor} {product}").trim().to_string()
}

fn gpu_driver() -> Option<String> {
    for entry in std::fs::read_dir("/sys/class/drm").ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("card") && !name.contains('-') {
            let link = std::fs::read_link(entry.path().join("device/driver")).ok()?;
            return link.file_name().map(|n| clean(&n.to_string_lossy()));
        }
    }
    None
}

fn connectors() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/sys/class/drm")
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((_, conn)) = name.split_once('-') else {
            continue;
        };
        let base = entry.path();
        if read(&base.join("status").to_string_lossy()) == "connected" {
            // First line = preferred mode; clean() only after splitting (it maps '\n' to '?').
            let modes = std::fs::read_to_string(base.join("modes")).unwrap_or_default();
            out.push((clean(conn), clean(modes.lines().next().unwrap_or("?"))));
        }
    }
    out
}

fn live_display(display: u16) {
    let conn = match xserver::connect_user(display) {
        Ok(c) => c,
        Err(err) => return println!("{KO} :{display}: {err}"),
    };
    let report = || -> Result<(), String> {
        let root = conn.setup().roots.first().ok_or("no screen")?.root;
        let res = conn
            .randr_get_screen_resources(root)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        for &o in &res.outputs {
            let info = conn
                .randr_get_output_info(o, res.config_timestamp)
                .map_err(|e| e.to_string())?
                .reply()
                .map_err(|e| e.to_string())?;
            if info.connection != randr::Connection::CONNECTED {
                continue;
            }
            let name = clean(&String::from_utf8_lossy(&info.name));
            let size = |id: u32| {
                res.modes
                    .iter()
                    .find(|m| m.id == id)
                    .map(|m| format!("{}x{}", m.width, m.height))
            };
            let pref = info
                .modes
                .first()
                .filter(|_| info.num_preferred > 0)
                .and_then(|&m| size(m));
            let cur = if info.crtc == 0 {
                None
            } else {
                let ci = conn
                    .randr_get_crtc_info(info.crtc, res.config_timestamp)
                    .map_err(|e| e.to_string())?
                    .reply()
                    .map_err(|e| e.to_string())?;
                (ci.mode != 0).then(|| format!("{}x{}", ci.width, ci.height))
            };
            match (cur, pref) {
                (Some(c), Some(p)) if c == p => println!("{OK} :{display} {name}: {c} (fitted)"),
                (c, p) => println!(
                    "{KO} :{display} {name}: current {}, host wants {}",
                    c.as_deref().unwrap_or("off"),
                    p.as_deref().unwrap_or("?")
                ),
            }
        }
        Ok(())
    };
    if let Err(err) = report() {
        println!("{KO} :{display}: {err}");
    }
}

fn spice_agents() -> usize {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            std::fs::read_to_string(e.path().join("comm"))
                .is_ok_and(|c| c.trim() == "spice-vdagent")
        })
        .count()
}

pub fn run() {
    println!(
        "kvm-autofit {} (rust) — guest display diagnosis",
        env!("CARGO_PKG_VERSION")
    );

    let virt = virtualization();
    if virt.contains("QEMU") || virt.contains("KVM") || virt.contains("Red Hat") {
        println!("{OK} virtualization: {virt}");
    } else {
        println!(
            "{KO} virtualization: {} (expected QEMU/KVM)",
            if virt.is_empty() { "unknown" } else { &virt }
        );
    }

    match gpu_driver().as_deref() {
        Some("virtio-pci" | "virtio_gpu") => println!("{OK} GPU: virtio-gpu"),
        Some("qxl") => println!("{OK} GPU: qxl (virtio is recommended)"),
        other => println!(
            "{KO} GPU driver: {} (set Video model = Virtio on the host)",
            other.unwrap_or("unknown")
        ),
    }
    for (conn, mode) in connectors() {
        println!("{OK} connector {conn}: connected, host wants {mode}");
    }

    match std::env::var("DISPLAY")
        .ok()
        .and_then(|d| xserver::parse_display(&d))
    {
        Some(d) => live_display(d),
        None => println!(
            "{NA} no $DISPLAY: run doctor from a terminal inside the VM to check the live display"
        ),
    }

    let n = spice_agents();
    if n > 1 {
        println!("{KO} {n} spice-vdagent session agents running (expected 1 per session)");
    }

    let unit = Path::new("/etc/systemd/system/kvm-autofit.service");
    let unit_text = std::fs::read_to_string(unit).unwrap_or_default();
    if unit_text.contains("kvm-autofit watch") {
        println!("{OK} systemd unit installed (rust watch daemon)");
    } else if unit.exists() {
        println!("{NA} systemd unit is the bash version (oneshot)");
    } else {
        println!("{KO} systemd unit missing (sudo ./install.sh)");
    }
    if Path::new("/etc/udev/rules.d/90-kvm-autofit.rules").exists()
        && unit_text.contains("kvm-autofit watch")
    {
        println!(
            "{KO} bash udev rule still present: events handled twice (re-run sudo ./install.sh)"
        );
    }
    if Path::new("/etc/lightdm").is_dir() {
        if Path::new("/etc/lightdm/lightdm.conf.d/50-kvm-autofit.conf").exists() {
            println!("{OK} LightDM hooks installed");
        } else {
            println!("{KO} LightDM hooks missing (sudo ./install.sh)");
        }
    }
    println!("     logs: journalctl -t kvm-autofit -b");
    println!(
        "     host: virt-manager → View → Scale Display → 'Auto resize VM with window' must be ON"
    );
}
