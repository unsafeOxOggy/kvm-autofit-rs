# kvm-autofit (édition Rust)

Même but que [`kvm-autofit`](../kvm-autofit) (bash) : l'écran des **VM Linux KVM/QEMU** suit la
fenêtre de l'hôte au démarrage, au greeter, au login et en plein écran. Mêmes commandes, mêmes
chemins d'installation : **les deux éditions sont interchangeables**, chaque `install.sh`
remplace l'autre.

## Différences avec l'édition bash

| | bash | rust |
|---|---|---|
| Détection des redimensionnements | règle udev → service oneshot | démon `watch` (netlink noyau, sans libudev) |
| Application | `xrandr --auto` (dépendance) | RandR natif via `x11rb` (aucune dépendance) |
| Livraison | script, modifiable dans la VM | binaire statique musl ~840 Kio, compilé sur l'hôte |
| Fichiers installés | script + règle udev + unité + hook LightDM | binaire + unité + hook LightDM |
| Sécurité | validations shell | voir ci-dessous + `#![forbid(unsafe_code)]`, tests |

## Construire (sur l'hôte) puis installer (dans la VM)

```bash
./build.sh                       # tests + clippy -D warnings + binaire musl + sha256
rsync -az --exclude=target --exclude=.git ./ kali@<vm>:~/kvm-autofit-rs/
# dans la VM :
cd ~/kvm-autofit-rs && sudo ./install.sh     # --dry-run, --no-lightdm, --uninstall
kvm-autofit doctor
```

Retour à l'édition bash : `sudo ./install.sh --uninstall` puis `sudo ~/kvm-autofit/install.sh`.

## Commandes

| Commande | Rôle |
|---|---|
| `kvm-autofit [--settle]` | ajuste `$DISPLAY` (utilisateur) |
| `kvm-autofit --display :0` | ajuste un display précis |
| `sudo kvm-autofit --all` | ajuste tous les serveurs X |
| `kvm-autofit watch` | démon (lancé par systemd) |
| `kvm-autofit display-setup` | hook LightDM |
| `kvm-autofit doctor` | diagnostic |

Réglages settle : `KVM_AUTOFIT_ROUNDS` (1–64, défaut 16), `KVM_AUTOFIT_DELAY` (0.05–2 s, défaut 0.25).
Logs : `journalctl -t kvm-autofit -u kvm-autofit -b`.

## Modèle de sécurité

Le démon tourne en root et lit des données que n'importe quel utilisateur peut influencer.

| Entrée | Menace | Défense |
|---|---|---|
| `/proc/<pid>/cmdline` de `Xorg` (`-auth`) | faux `Xorg :5 -auth /etc/shadow` pour faire lire un fichier par root | le fichier n'est lu que s'il appartient à l'uid du serveur X ; `O_NOFOLLOW`, fichier régulier, ≤ 64 Kio |
| socket `/tmp/.X11-unix/X<n>` (dossier 1777) | faux serveur qui récupère le cookie | `SO_PEERCRED` : l'uid du pair doit être celui du Xorg découvert |
| chemin `-auth`/`XAUTHORITY` vers un périphérique | `open()` par root avec effets de bord | `lstat` avant `open()`, puis `fstat` : même dev/inode |
| Xauthority | fichier piégé | parseur borné sans panique, seul `MIT-MAGIC-COOKIE-1` de 16 octets, cookie jamais journalisé |
| `/proc/<pid>/environ` (repli) | `XAUTHORITY` pointant ailleurs | fichier du client lui-même ; jamais vers un serveur X non-root d'un autre uid |
| netlink uevent | faux événements / tempête | seulement pid 0 (noyau), messages ≤ 8 Kio non tronqués, `SUBSYSTEM=drm` + `ACTION=change` + `dri/card*`, regroupement 150 ms et limite 1 recadrage / 300 ms |
| arguments / env | injection | `--display` strict (`:N`), env bornée, aucun shell, ré-exécution avec env vidée |
| logs | injection de lignes | caractères de contrôle neutralisés, 512 caractères max |
| géométrie X | débordement | `checked_add`, `try_from`, bornes `GetScreenSizeRange` |
| processus | élévation | unité systemd durcie (`systemd-analyze security` : 2.1 OK) : 2 capacités seulement, pas de réseau IP, FS en lecture seule sauf `/run`, filtre d'appels système |

## Risques résiduels connus

- Un utilisateur local peut lancer un serveur X à lui qui ne répond plus : le démon reste bloqué
  sur ce serveur (disponibilité du recadrage seulement, aucune élévation). Atténué : serveurs root
  traités en premier, 8 serveurs max par passe ; `Restart=on-failure` ne couvre pas un blocage.
- Le `sha256` livré avec le binaire garantit l'intégrité de la copie, pas son authenticité
  (pas de signature).

## Limites

X11 uniquement (Wayland gère seul). x86_64 uniquement pour le binaire fourni (autre arch :
`cargo build --release --target <arch>-unknown-linux-musl`). Greeters GDM/SDDM : seul le démon
couvre les redimensionnements.
