# kvm-autofit-rs — contexte projet (handoff pour Claude Code)

> Relu au démarrage d'une session Claude Code dans ce dépôt. Mettre à jour quand l'état change.

## But

Réécriture Rust de `~/DEV/kvm-autofit` (bash, qui marche et reste maintenu). L'utilisateur veut
**le choix** entre les deux : mêmes commandes et chemins d'installation, chaque `install.sh`
remplace l'autre (le bash désactive le démon rust ; le rust supprime la règle udev bash).

## Architecture

- `src/main.rs` — CLI clap (compatible bash : `--all --display --settle --display-setup` +
  sous-commandes `watch`, `display-setup`, `doctor`), verrou `/run/kvm-autofit.lock` (flock,
  O_NOFOLLOW, 0600), ré-exec du hook LightDM avec env vidée + `process_group(0)`.
- `src/fit.rs` — `xrandr --auto` natif : `GetScreenResources` (re-probe), `plan()` pur et testé,
  apply = désactiver CRTC hors plan/débordants → `SetScreenSize` (DPI conservé) → `SetCrtcConfig`.
  **Taille courante via `GetGeometry`** (`setup()` est figé à la connexion : bug corrigé).
- `src/xserver.rs` — découverte `/proc` (`comm` = Xorg/X), `-auth`, repli environ client,
  connexion avec contrôle `SO_PEERCRED`.
- `src/xauth.rs` — parseur Xauthority borné + `read_checked` (règles de propriétaire).
- `src/watch.rs` — netlink `KOBJECT_UEVENT` groupe 1, pid 0 uniquement, coalescence + rate limit.
- `src/doctor.rs`, `src/log.rs` (stderr sous systemd, sinon /dev/log ; `clean()` anti-injection).
- `dist/systemd/kvm-autofit.service` (Type=simple, durci, score 2.1), `dist/lightdm/50-kvm-autofit.conf`.
- `build.sh` (hôte : test + clippy + musl + sha256 → `dist/bin/`), `install.sh` (VM : vérifie ELF,
  arch, sha256, `--version` avant d'installer).

## Règles de sécurité du projet (ne pas régresser)

`#![forbid(unsafe_code)]` ; aucune exécution de shell ; toute donnée de /proc, X, netlink, env ou
fichier est non fiable → parse borné, pas d'`unwrap`/index en prod (lints clippy actifs) ; un cookie
n'est lu que si le fichier appartient à l'uid du serveur X, et n'est envoyé qu'après `SO_PEERCRED` ;
jamais de secret dans les logs (`Cookie` a un `Debug` masqué). Vérifier avant de livrer :
`./build.sh` (tests + clippy -D warnings), `cargo audit`, `systemd-analyze security` sur l'unité.

## VM de test

`ssh kali@192.168.100.18` (clé). Tester sans sudo : `scp dist/bin/kvm-autofit kali@…:/tmp/` puis
`DISPLAY=:0 XAUTHORITY=~/.Xauthority /tmp/kvm-autofit --settle -v` après
`xrandr --output Virtual-1 --mode 1920x1440`. sudo = action de l'utilisateur.

## État (MAJ 2026-10-16)

- ✅ v0.1.0 : 14 tests, clippy -D warnings, cargo audit OK (30 crates, 0 vuln), unité 2.1 OK.
- ✅ VM (utilisateur) : 1920x1440 → 1920x1025 en 0.94 s, no-op 0.5 s, entrées invalides refusées.
- ⬜ Chemin root non testé (démon `watch`, découverte + peercred + cookie root, durcissement systemd)
  → nécessite `sudo ./install.sh` par l'utilisateur. La VM a actuellement l'édition **bash** installée.
- ⬜ Pas de remote GitHub.

## TODO

1. Après install root : vérifier `systemctl status kvm-autofit`, redimensionnement à chaud,
   greeter au boot ; ajuster `SystemCallFilter` si un appel est refusé (EPERM dans les logs).
2. Hook post-resize côté session (wallpaper/polybar), exécuté en tant qu'utilisateur.
3. Tests d'intégration avec Xvfb + RandR (CI).
4. Paquets `.deb`/`.rpm` (cargo-deb / cargo-generate-rpm).
