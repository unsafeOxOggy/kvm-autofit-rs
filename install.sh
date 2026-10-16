#!/usr/bin/env bash
# kvm-autofit (rust) installer — sudo ./install.sh [--uninstall] [--dry-run] [--no-lightdm]
# Installs the prebuilt static binary from dist/bin/ (made on the host by ./build.sh).
# Replaces the bash edition if present (same command name and paths).
set -euo pipefail

ROOT="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
SRC_BIN="$ROOT/dist/bin/kvm-autofit"
LIBEXEC=/usr/local/libexec/kvm-autofit
BIN=/usr/local/bin/kvm-autofit
DOC=/usr/local/share/doc/kvm-autofit
UNIT=/etc/systemd/system/kvm-autofit.service
LIGHTDM=/etc/lightdm/lightdm.conf.d/50-kvm-autofit.conf
BASH_UDEV=/etc/udev/rules.d/90-kvm-autofit.rules   # bash edition only

DRY=0 UNINSTALL=0 WITH_LIGHTDM=1
for a in "$@"; do
    case $a in
        --dry-run)    DRY=1 ;;
        --uninstall)  UNINSTALL=1 ;;
        --no-lightdm) WITH_LIGHTDM=0 ;;
        -h|--help)    sed -n 2,4p "$0"; exit 0 ;;
        *)            echo "unknown option: $a" >&2; exit 2 ;;
    esac
done

info() { printf '\033[32m•\033[0m %s\n' "$*"; }
warn() { printf '\033[33m▲\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[31m✗\033[0m %s\n' "$*" >&2; exit 1; }
run()  { if (( DRY )); then printf '  [dry-run] %s\n' "$*"; else "$@"; fi; }

(( DRY || EUID == 0 )) || die "run with sudo: sudo $0 $*"
umask 022

stop_unit() {
    if systemctl is-enabled -q kvm-autofit.service 2>/dev/null || systemctl is-active -q kvm-autofit.service 2>/dev/null; then
        run systemctl disable --now kvm-autofit.service
    fi
}

if (( UNINSTALL )); then
    stop_unit
    run rm -f "$UNIT" "$LIGHTDM" "$BIN" "$LIBEXEC" "$BASH_UDEV"
    run rm -rf "$DOC"
    run systemctl daemon-reload
    run udevadm control --reload
    info "kvm-autofit removed (LightDM picks the change up at next restart)"
    exit 0
fi

# --- binary integrity: present, static ELF x86-64, checksum from build.sh matches
[[ -f $SRC_BIN && ! -L $SRC_BIN ]] || die "missing $SRC_BIN — run ./build.sh on the host, then copy the folder"
[[ "$(head -c4 "$SRC_BIN" | od -An -tx1 | tr -d ' \n')" == 7f454c46 ]] || die "$SRC_BIN is not an ELF binary"
[[ "$(uname -m)" == x86_64 ]] || die "binary is built for x86_64, this machine is $(uname -m)"
if [[ -f $SRC_BIN.sha256 ]]; then
    (cd "$(dirname "$SRC_BIN")" && sha256sum --quiet -c kvm-autofit.sha256) || die "checksum mismatch: corrupted copy?"
else
    warn "no checksum file next to the binary — integrity not verified"
fi
"$SRC_BIN" --version >/dev/null || die "binary does not run on this system"

virt="$(systemd-detect-virt --vm 2>/dev/null || true)"
[[ $virt == kvm || $virt == qemu ]] || warn "not a KVM guest (detected: ${virt:-none}) — installing anyway"

# --- replace the bash edition if installed (its udev rule would double-trigger)
if [[ -f $BASH_UDEV ]]; then
    info "removing bash edition udev rule ($BASH_UDEV)"
    run rm -f "$BASH_UDEV"
    run udevadm control --reload
fi
stop_unit

# --- files (root:root, not writable by others)
info "installing $LIBEXEC"
run install -o root -g root -Dm755 "$SRC_BIN" "$LIBEXEC"
run ln -sfn "$LIBEXEC" "$BIN"
[[ -f $ROOT/README.md ]] && run install -o root -g root -Dm644 "$ROOT/README.md" "$DOC/README.md"

info "installing systemd unit $UNIT"
run install -o root -g root -Dm644 "$ROOT/dist/systemd/kvm-autofit.service" "$UNIT"

if (( WITH_LIGHTDM )) && [[ -d /etc/lightdm ]]; then
    other="$(grep -rlsE '^\s*(display|session)-setup-script' /etc/lightdm/lightdm.conf /etc/lightdm/lightdm.conf.d/ \
             | grep -vxF "$LIGHTDM" || true)"
    if [[ -n $other ]]; then
        warn "LightDM already defines a setup script in: $other"
        warn "skipping LightDM hooks — call '$LIBEXEC display-setup' from that script instead"
    else
        info "installing LightDM hooks $LIGHTDM"
        run install -o root -g root -Dm644 "$ROOT/dist/lightdm/50-kvm-autofit.conf" "$LIGHTDM"
    fi
fi

run systemctl daemon-reload
run systemctl enable --now kvm-autofit.service

cat <<EOF

done. Check with:   kvm-autofit doctor        (from a terminal inside the VM)
Daemon status:      systemctl status kvm-autofit
Logs:               journalctl -t kvm-autofit -u kvm-autofit -b
Back to bash:       sudo ./install.sh --uninstall   then   sudo ~/kvm-autofit/install.sh
EOF
