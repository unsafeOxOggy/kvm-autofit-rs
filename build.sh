#!/usr/bin/env bash
# Build the static binary on the host (VMs need no Rust toolchain): ./build.sh
# Output: dist/bin/kvm-autofit + dist/bin/kvm-autofit.sha256
set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")"
TARGET=x86_64-unknown-linux-musl
rustup target list --installed | grep -qx "$TARGET" || rustup target add "$TARGET"
cargo test --quiet
cargo clippy --quiet --all-targets -- -D warnings
cargo build --release --locked --target "$TARGET"
install -Dm755 "target/$TARGET/release/kvm-autofit" dist/bin/kvm-autofit
(cd dist/bin && sha256sum kvm-autofit > kvm-autofit.sha256)
echo "built: dist/bin/kvm-autofit ($(du -h dist/bin/kvm-autofit | cut -f1))"
echo "deploy: rsync -az --exclude=target --exclude=.git ./ <user>@<vm>:~/kvm-autofit-rs/"
