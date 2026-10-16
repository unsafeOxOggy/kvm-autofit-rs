//! Native RandR equivalent of `xrandr --output … --auto` for every connected output.
//!
//! `GetScreenResources` (not the `Current` variant) makes X re-probe connectors,
//! which is what picks up the new preferred mode published by virtio-gpu / qxl.

use std::collections::{HashMap, HashSet};
use std::thread::sleep;
use std::time::Duration;

use x11rb::CURRENT_TIME;
use x11rb::connection::Connection;
use x11rb::protocol::randr::{self, ConnectionExt as _, Rotation, SetConfig};
use x11rb::protocol::xproto::ConnectionExt as _;

use crate::info;

/// Settle tuning (bounded; see `Settle::from_env`).
#[derive(Debug, Clone, Copy)]
pub struct Settle {
    pub rounds: u32,
    pub delay: Duration,
}

impl Settle {
    pub const ONCE: Settle = Settle {
        rounds: 1,
        delay: Duration::ZERO,
    };
    const STABLE_NEEDED: u32 = 3;

    /// KVM_AUTOFIT_ROUNDS (1..=64, default 16) and KVM_AUTOFIT_DELAY in seconds
    /// (0.05..=2.0, default 0.25). Out-of-range or malformed values fall back to defaults.
    pub fn from_env() -> Settle {
        let rounds = std::env::var("KVM_AUTOFIT_ROUNDS")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|r| (1..=64).contains(r))
            .unwrap_or(16);
        let delay = std::env::var("KVM_AUTOFIT_DELAY")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|d| (0.05..=2.0).contains(d))
            .unwrap_or(0.25);
        Settle {
            rounds,
            delay: Duration::from_secs_f64(delay),
        }
    }
}

/// A connected output as seen by the planner.
#[derive(Debug, Clone)]
pub struct OutIn {
    pub id: u32,
    pub name: String,
    pub cur_crtc: u32,
    pub crtcs: Vec<u32>,
    pub mode: u32,
    pub w: u16,
    pub h: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub output: u32,
    pub crtc: u32,
    pub mode: u32,
    pub x: i16,
    pub w: u16,
    pub h: u16,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    pub placements: Vec<Placement>,
    pub width: u16,
    pub height: u16,
}

/// Lay outputs left to right at their preferred mode. Pure: no X access.
/// Returns None when no CRTC is available or the geometry overflows.
pub fn plan(outs: &[OutIn]) -> Option<Plan> {
    let mut used: HashSet<u32> = HashSet::new();
    let mut placements = Vec::with_capacity(outs.len());
    let (mut width, mut height) = (0u16, 0u16);
    for o in outs {
        let crtc = if o.cur_crtc != 0 && !used.contains(&o.cur_crtc) {
            o.cur_crtc
        } else {
            *o.crtcs.iter().find(|c| **c != 0 && !used.contains(c))?
        };
        used.insert(crtc);
        let x = i16::try_from(width).ok()?;
        placements.push(Placement {
            output: o.id,
            crtc,
            mode: o.mode,
            x,
            w: o.w,
            h: o.h,
        });
        width = width.checked_add(o.w)?;
        height = height.max(o.h);
    }
    if placements.is_empty() || width == 0 || height == 0 {
        return None;
    }
    Some(Plan {
        placements,
        width,
        height,
    })
}

pub enum Outcome {
    Fitted,
    Changed(String),
}

type Res<T> = Result<T, String>;

fn e<E: std::fmt::Display>(what: &'static str) -> impl FnOnce(E) -> String {
    move |err| format!("{what}: {err}")
}

/// One pass on one connection.
pub fn fit_once<C: Connection>(conn: &C, screen: usize) -> Res<Outcome> {
    let root_screen = conn.setup().roots.get(screen).ok_or("no such screen")?;
    let root = root_screen.root;
    conn.randr_query_version(1, 3)
        .map_err(e("randr"))?
        .reply()
        .map_err(e("randr version"))?;

    let res = conn
        .randr_get_screen_resources(root)
        .map_err(e("resources"))?
        .reply()
        .map_err(e("resources"))?;
    let ts = res.config_timestamp;
    let modes: HashMap<u32, &randr::ModeInfo> = res.modes.iter().map(|m| (m.id, m)).collect();

    let mut outs = Vec::new();
    for &output in &res.outputs {
        let info = conn
            .randr_get_output_info(output, ts)
            .map_err(e("output"))?
            .reply()
            .map_err(e("output"))?;
        if info.connection != randr::Connection::CONNECTED || info.num_preferred == 0 {
            continue;
        }
        let Some(&mode) = info.modes.first() else {
            continue;
        };
        let Some(m) = modes.get(&mode) else { continue };
        outs.push(OutIn {
            id: output,
            name: String::from_utf8_lossy(&info.name).into_owned(),
            cur_crtc: info.crtc,
            crtcs: info.crtcs.clone(),
            mode,
            w: m.width,
            h: m.height,
        });
    }
    let Some(plan) = plan(&outs) else {
        return Ok(Outcome::Fitted); // nothing connected with a preferred mode: nothing to do
    };

    let mut crtcs = HashMap::new();
    for &c in &res.crtcs {
        let ci = conn
            .randr_get_crtc_info(c, ts)
            .map_err(e("crtc"))?
            .reply()
            .map_err(e("crtc"))?;
        crtcs.insert(c, ci);
    }

    let planned: HashMap<u32, &Placement> = plan.placements.iter().map(|p| (p.crtc, p)).collect();
    let crtc_ok = |p: &Placement| {
        crtcs
            .get(&p.crtc)
            .is_some_and(|c| c.mode == p.mode && c.x == p.x && c.y == 0 && c.outputs == [p.output])
    };
    let stray = crtcs
        .iter()
        .any(|(id, c)| c.mode != 0 && !planned.contains_key(id));
    // setup() is a snapshot from connect time: ask the server for the live size.
    let geo = conn
        .get_geometry(root)
        .map_err(e("geometry"))?
        .reply()
        .map_err(e("geometry"))?;
    let cur_size = (geo.width, geo.height);
    let size_ok = cur_size == (plan.width, plan.height);
    if size_ok && !stray && plan.placements.iter().all(crtc_ok) {
        return Ok(Outcome::Fitted);
    }

    let range = conn
        .randr_get_screen_size_range(root)
        .map_err(e("size range"))?
        .reply()
        .map_err(e("size range"))?;
    if plan.width < range.min_width
        || plan.width > range.max_width
        || plan.height < range.min_height
        || plan.height > range.max_height
    {
        return Err(format!(
            "{}x{} outside screen limits",
            plan.width, plan.height
        ));
    }

    let before: Vec<String> = outs
        .iter()
        .map(|o| {
            let cur = crtcs
                .get(&o.cur_crtc)
                .filter(|c| c.mode != 0)
                .map(|c| format!("{}x{}", c.width, c.height));
            format!(
                "{} {} -> {}x{}",
                o.name,
                cur.as_deref().unwrap_or("off"),
                o.w,
                o.h
            )
        })
        .collect();

    conn.grab_server().map_err(e("grab"))?;
    let target = ScreenTarget {
        root,
        cur_size,
        dpi: start_dpi(root_screen),
    };
    let applied = apply(conn, &target, ts, &plan, &crtcs, &planned);
    let _ = conn.ungrab_server();
    let _ = conn.flush();
    applied?;
    Ok(Outcome::Changed(before.join(", ")))
}

/// The root window being resized: live size and the DPI to preserve.
struct ScreenTarget {
    root: u32,
    cur_size: (u16, u16),
    dpi: f64,
}

/// DPI the server started with (setup() pixel/mm pair), 96 if missing or absurd.
fn start_dpi(scr: &x11rb::protocol::xproto::Screen) -> f64 {
    let dpi = if scr.width_in_millimeters > 0 {
        f64::from(scr.width_in_pixels) * 25.4 / f64::from(scr.width_in_millimeters)
    } else {
        96.0
    };
    if (30.0..=600.0).contains(&dpi) {
        dpi
    } else {
        96.0
    }
}

fn apply<C: Connection>(
    conn: &C,
    target: &ScreenTarget,
    ts: u32,
    plan: &Plan,
    crtcs: &HashMap<u32, randr::GetCrtcInfoReply>,
    planned: &HashMap<u32, &Placement>,
) -> Res<()> {
    let (w, h) = (i32::from(plan.width), i32::from(plan.height));
    // 1. Like xrandr: switch off CRTCs we don't use, or that won't fit the new screen.
    for (&id, c) in crtcs {
        let exceeds =
            i32::from(c.x) + i32::from(c.width) > w || i32::from(c.y) + i32::from(c.height) > h;
        if c.mode != 0 && (!planned.contains_key(&id) || exceeds) {
            let r = conn
                .randr_set_crtc_config(id, CURRENT_TIME, ts, 0, 0, 0, Rotation::ROTATE0, &[])
                .map_err(e("disable crtc"))?
                .reply()
                .map_err(e("disable crtc"))?;
            if r.status != SetConfig::SUCCESS {
                return Err(format!("disable crtc {id}: {:?}", r.status));
            }
        }
    }
    // 2. Resize the screen, keeping the DPI the server started with (96 if unknown).
    if target.cur_size != (plan.width, plan.height) {
        let dpi = target.dpi;
        let mm = |px: u16| {
            (f64::from(px) * 25.4 / dpi)
                .round()
                .clamp(1.0, f64::from(u32::MAX)) as u32
        };
        conn.randr_set_screen_size(
            target.root,
            plan.width,
            plan.height,
            mm(plan.width),
            mm(plan.height),
        )
        .map_err(e("screen size"))?
        .check()
        .map_err(e("screen size"))?;
    }
    // 3. Program each CRTC with its preferred mode.
    for p in &plan.placements {
        let r = conn
            .randr_set_crtc_config(
                p.crtc,
                CURRENT_TIME,
                ts,
                p.x,
                0,
                p.mode,
                Rotation::ROTATE0,
                &[p.output],
            )
            .map_err(e("set crtc"))?
            .reply()
            .map_err(e("set crtc"))?;
        if r.status != SetConfig::SUCCESS {
            return Err(format!("set crtc {}: {:?}", p.crtc, r.status));
        }
    }
    if let Some(first) = plan.placements.first() {
        let _ = conn.randr_set_output_primary(target.root, first.output);
    }
    Ok(())
}

/// Run passes until the layout has been stable `STABLE_NEEDED` times (or rounds run out).
pub fn fit_settle<C: Connection>(conn: &C, label: &str, settle: Settle) -> Res<()> {
    let mut stable = 0;
    for round in 0..settle.rounds {
        match fit_once(conn, 0)? {
            Outcome::Fitted => {
                stable += 1;
                if stable >= Settle::STABLE_NEEDED {
                    return Ok(());
                }
            }
            Outcome::Changed(what) => {
                stable = 0;
                info!("{label}: {what}");
            }
        }
        if round + 1 < settle.rounds {
            sleep(settle.delay);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(id: u32, cur: u32, crtcs: &[u32], w: u16, h: u16) -> OutIn {
        OutIn {
            id,
            name: format!("o{id}"),
            cur_crtc: cur,
            crtcs: crtcs.to_vec(),
            mode: 100 + id,
            w,
            h,
        }
    }

    #[test]
    fn single_head_keeps_its_crtc() {
        let p = plan(&[out(1, 10, &[10, 11], 1920, 1025)]).unwrap();
        assert_eq!((p.width, p.height), (1920, 1025));
        assert_eq!(p.placements[0].crtc, 10);
        assert_eq!(p.placements[0].x, 0);
    }

    #[test]
    fn multi_head_left_to_right_with_distinct_crtcs() {
        let p = plan(&[
            out(1, 0, &[10, 11], 1920, 1080),
            out(2, 0, &[10, 11], 1280, 1440),
        ])
        .unwrap();
        assert_eq!((p.width, p.height), (3200, 1440));
        assert_eq!(p.placements[1].x, 1920);
        assert_ne!(p.placements[0].crtc, p.placements[1].crtc);
    }

    #[test]
    fn rejects_overflow_missing_crtc_and_empty() {
        assert!(plan(&[out(1, 0, &[10], 40000, 10), out(2, 0, &[11], 40000, 10)]).is_none());
        assert!(plan(&[out(1, 0, &[10], 100, 100), out(2, 0, &[10], 100, 100)]).is_none());
        assert!(plan(&[]).is_none());
        assert!(plan(&[out(1, 0, &[10], 0, 0)]).is_none());
    }

    #[test]
    fn settle_env_is_bounded() {
        // from_env reads the process env; only check the defaults path here.
        let s = Settle::from_env();
        assert!((1..=64).contains(&s.rounds));
        assert!(s.delay >= Duration::from_millis(50) && s.delay <= Duration::from_secs(2));
    }
}
