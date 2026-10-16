//! kvm-autofit - fit X displays to the size the KVM host asks for (Rust edition).
//!
//! Same CLI as the bash version, plus `watch` (netlink daemon, replaces the udev rule).
#![forbid(unsafe_code)]

mod doctor;
mod fit;
mod log;
mod watch;
mod xauth;
mod xserver;

use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode, Stdio};

use clap::{Parser, Subcommand};
use rustix::fd::OwnedFd;
use rustix::fs::{FlockOperation, Mode, OFlags};

use fit::Settle;

const LOCK_PATH: &str = "/run/kvm-autofit.lock";

#[derive(Parser)]
#[command(version, about = "Fit X displays to the KVM host window")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Fit every running X server (root)
    #[arg(short, long)]
    all: bool,
    /// Fit only this display, e.g. :0
    #[arg(short, long, value_parser = display_arg)]
    display: Option<u16>,
    /// Keep re-checking for a few seconds (host resizes arrive in steps)
    #[arg(short, long)]
    settle: bool,
    /// LightDM hook (same as the `display-setup` subcommand)
    #[arg(long, hide = true)]
    display_setup: bool,
    /// Also print log lines to stderr
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Daemon: refit on every DRM change uevent (root, used by the systemd unit)
    Watch,
    /// LightDM display-setup/session-setup hook: fit now, settle in the background
    DisplaySetup,
    /// Diagnose the guest display stack
    Doctor,
}

fn display_arg(s: &str) -> Result<u16, String> {
    xserver::parse_display(s).ok_or_else(|| "expected a local display like :0".into())
}

fn is_root() -> bool {
    rustix::process::geteuid().is_root()
}

/// Serialize root runs (daemon, LightDM hook, manual --all) on a root-owned lock.
fn lock() -> Option<OwnedFd> {
    let fd = rustix::fs::open(
        LOCK_PATH,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .ok()?;
    rustix::fs::flock(&fd, FlockOperation::LockExclusive).ok()?;
    Some(fd)
}

/// Root: fit every discovered server (optionally only `only`).
fn fit_servers(only: Option<u16>, settle: Settle) -> bool {
    let _guard = lock();
    let servers: Vec<_> = xserver::discover()
        .into_iter()
        .filter(|s| only.is_none_or(|d| s.display == d))
        .collect();
    if servers.is_empty() {
        let which = only.map(|d| format!(" on :{d}")).unwrap_or_default();
        info!("no X server found{which} (Wayland sessions resize natively)");
        return only.is_none();
    }
    let mut ok = true;
    for srv in servers {
        let label = format!(":{}", srv.display);
        let result =
            xserver::connect_server(&srv).and_then(|c| fit::fit_settle(&c, &label, settle));
        if let Err(err) = result {
            info!("{label}: {err}");
            ok = false;
        }
    }
    ok
}

/// User: fit $DISPLAY / --display with the user's own credentials.
fn fit_user(display: u16, settle: Settle) -> bool {
    let label = format!(":{display}");
    match xserver::connect_user(display).and_then(|c| fit::fit_settle(&c, &label, settle)) {
        Ok(()) => true,
        Err(err) => {
            info!("{label}: {err}");
            false
        }
    }
}

fn env_display() -> Option<u16> {
    std::env::var("DISPLAY")
        .ok()
        .and_then(|d| xserver::parse_display(&d))
}

/// LightDM hook: one pass now (LightDM waits for us), settle in a detached child.
fn display_setup(verbose: bool) -> bool {
    let Some(display) = env_display() else {
        info!("display-setup: no valid $DISPLAY");
        return false;
    };
    let ok = if is_root() {
        fit_servers(Some(display), Settle::ONCE)
    } else {
        fit_user(display, Settle::ONCE)
    };
    // Re-exec ourselves with a clean environment and fixed arguments (no shell).
    if let Ok(exe) = std::env::current_exe() {
        let mut cmd = Command::new(exe);
        cmd.args(["--display", &format!(":{display}"), "--settle"])
            .env_clear()
            .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        if verbose {
            cmd.arg("--verbose");
        }
        if !is_root()
            && let Some(a) = std::env::var_os("XAUTHORITY")
        {
            cmd.env("XAUTHORITY", a);
        }
        if let Err(err) = cmd.spawn() {
            info!("display-setup: cannot start settle pass: {err}");
        }
    }
    ok
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    log::set_verbose(cli.verbose);
    let settle = if cli.settle {
        Settle::from_env()
    } else {
        Settle::ONCE
    };

    let ok = match cli.cmd {
        Some(Cmd::Doctor) => {
            doctor::run();
            true
        }
        Some(Cmd::Watch) => {
            if !is_root() {
                eprintln!("kvm-autofit: watch must run as root (systemd unit)");
                return ExitCode::FAILURE;
            }
            let settle = Settle::from_env();
            if let Err(err) = watch::run(|| {
                fit_servers(None, settle);
            }) {
                info!("cannot listen to uevents: {err}");
                return ExitCode::FAILURE;
            }
            true
        }
        Some(Cmd::DisplaySetup) => display_setup(cli.verbose),
        None if cli.display_setup => display_setup(cli.verbose),
        None if cli.all => {
            if !is_root() {
                eprintln!("kvm-autofit: --all needs root (run without --all to fit $DISPLAY)");
                return ExitCode::FAILURE;
            }
            fit_servers(None, settle)
        }
        None => match cli.display.or_else(env_display) {
            Some(d) if is_root() => fit_servers(Some(d), settle),
            Some(d) => fit_user(d, settle),
            None if is_root() => fit_servers(None, settle),
            None => {
                eprintln!("kvm-autofit: no $DISPLAY (use --display :N, or run as root with --all)");
                return ExitCode::FAILURE;
            }
        },
    };
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
