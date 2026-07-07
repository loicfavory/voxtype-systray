# voxtype-systray

Icône StatusNotifierItem (systray) pour KDE Plasma Wayland affichant l'état du daemon **Voxtype**, avec une fenêtre de réglages pour les paramètres essentiels.

## Modes

| Commande | Effet |
|---------|-------|
| `voxtype-systray` | Mode tray : icône d'état dans le panel |
| `voxtype-systray settings` | Fenêtre de réglages egui |

## États de l'icône

| Couleur | Signification |
|---------|---------------|
| Rouge désaturé | Daemon arrêté / injoignable |
| Gris | Daemon actif, au repos (ni dictée ni réunion) |
| Vert désaturé | Enregistrement / transcription **ou** réunion en cours |

L'icône se met à jour en < 2 s. Le tooltip au survol distingue « Réunion en cours (recording) » de « En cours : Enregistrement ».

## Menu clic-droit

Le menu est **dynamique** : il reflète l'état courant à chaque ouverture.

### En-tête d'état (non cliquable)

| État | Libellé |
|------|---------|
| Daemon arrêté | `✕ Daemon arrêté` |
| Daemon actif, au repos | `○ Au repos` |
| Réunion en cours | `● Réunion en cours` |
| Dictée en cours | `● Dictée en cours` |

### Contrôle réunion

- **`▶ Démarrer une réunion`** (daemon actif, pas de réunion) — lance `voxtype meeting start --title "Réunion du JJ/MM/AA à HH:MM"` avec un titre horodaté à l'instant du clic
- **`■ Arrêter la réunion`** (réunion active) — lance `voxtype meeting stop`
- Grisé si le daemon est arrêté

### Contrôle dictée

- **`🎙 Démarrer la dictée`** (daemon actif, pas de dictée) — lance `voxtype record start`
- **`■ Arrêter la dictée`** (dictée active) — lance `voxtype record stop`
- Grisé si le daemon est arrêté

### Contrôle daemon

- **`⏼ Démarrer Voxtype`** (daemon arrêté) — lance `systemctl --user start voxtype`
- **`↻ Redémarrer Voxtype`** (daemon actif) — lance `systemctl --user restart voxtype`

### Autres

- **`Réglages…`** : ouvre la fenêtre de réglages (nouveau process `voxtype-systray settings`)
- **`Quitter`** : arrête le tray

> Les actions (démarrer/arrêter réunion, dictée, daemon) sont exécutées de façon non-bloquante (thread détaché). L'icône se met à jour dans les ~2 s via le poll existant.

## Fenêtre de réglages

Édite `~/.config/voxtype/config.toml` de manière non-destructive (commentaires et autres clés préservés) :

- **Dossier de stockage des réunions** (`[meeting].storage_path`) — champ texte + bouton « Parcourir… » (dialogue natif via XDG Desktop Portal)
- **Conserver l'audio des réunions** (`[meeting].retain_audio`) — case à cocher

À l'enregistrement :
1. Backup vers `config.toml.bak` avant toute modification
2. Écriture non-destructive via `toml_edit` (seules les deux clés sont modifiées)
3. Création du dossier de stockage si absent
4. Redémarrage du daemon via `systemctl --user restart voxtype`
5. Retour visuel succès/échec dans la fenêtre

## Dépendances système

- `voxtype` dans le `PATH` (le daemon doit être accessible via `voxtype status`)
- `systemctl --user` disponible (systemd user session)
- Session D-Bus active (requis par le protocole StatusNotifierItem / SNI)
- KDE Plasma ou tout panel compatible StatusNotifierItem (SNI)
- Pour le dialogue de dossier natif : portail XDG actif (KDE, GNOME ou `xdg-desktop-portal` + backend)

Aucune dépendance système Rust supplémentaire : le binaire est autosuffisant (icônes embarquées, polices embarquées).

## Build

```bash
# Dépendances : Rust stable >= 1.82 (edition 2024 + let-chains)
cargo build --release
# Le binaire se trouve dans target/release/voxtype-systray
```

## Lancement

```bash
# Mode tray
./target/release/voxtype-systray

# Fenêtre de réglages seule (sans tray)
./target/release/voxtype-systray settings
```

Pour activer les logs de debug (mode tray uniquement) :

```bash
RUST_LOG=voxtype_systray=debug ./target/release/voxtype-systray
```

Le process tourne en premier plan. Pour qu'il démarre automatiquement à l'ouverture de session, voir **Installation & autostart** ci-dessous.

## Installation & autostart

Le tray s'installe en tant que paquet Arch (`voxtype-systray-git`) géré par pacman. Le binaire est placé dans `/usr/bin` et l'autostart passe par un service **systemd --user** (`voxtype-systray.service`). La désinstallation est propre via `pacman -R`.

> Historique : l'autostart utilisait auparavant un fichier `.desktop` dans `/etc/xdg/autostart` (US-04). Abandonné car son `Exec=%h/.local/bin/...` reposait sur `%h`, qui n'est **pas** un field-code valide de la spec Desktop Entry — `systemd-xdg-autostart-generator` (utilisé par KDE Plasma/Wayland) ignorait donc silencieusement l'entrée à chaque démarrage de session. `%h` est en revanche un spécificateur systemd valide, d'où le passage à une unité `systemd --user`.

### Installation via paquet (recommandée — Arch Linux)

```bash
# Depuis le répertoire packaging/ du repo
cd packaging
makepkg -si
```

Le paquet installe :
- `/usr/bin/voxtype-systray` — le binaire
- `/usr/lib/systemd/user/voxtype-systray.service` — unité systemd --user

Contrairement à l'ancien autostart XDG, une unité systemd `--user` livrée par un paquet n'est **pas activée automatiquement**. Il faut l'activer une fois par utilisateur :

```bash
systemctl --user enable --now voxtype-systray.service
```

Le tray démarrera ensuite automatiquement à chaque ouverture de session graphique (`graphical-session.target`).

### Désinstallation

```bash
systemctl --user disable --now voxtype-systray.service
pacman -R voxtype-systray-git
```

Tout est retiré proprement (binaire + unité systemd).

### Désactiver / réactiver l'autostart sans désinstaller

```bash
# Désactiver (le binaire reste installé)
systemctl --user disable --now voxtype-systray.service

# Réactiver
systemctl --user enable --now voxtype-systray.service
```

Diagnostic : `systemctl --user status voxtype-systray.service` et `journalctl --user -u voxtype-systray.service`.

### Installation automatisée (dotfiles)

Le script `packages/80-voxtype.sh` des dotfiles fait tout cela automatiquement : il clone/met à jour ce dépôt depuis GitHub, construit le paquet via `makepkg -si`, et gère la migration depuis une éventuelle installation manuelle antérieure. Idempotent (relançable sans risque).

### Installation manuelle (sans paquet)

Pour une install rapide hors gestion de paquets (développement, test) :

```bash
# 1. Build
cargo build --release

# 2. Installer le binaire dans ~/.local/bin
install -Dm755 target/release/voxtype-systray ~/.local/bin/voxtype-systray

# 3. Installer et activer le service systemd --user
install -Dm644 assets/voxtype-systray.service ~/.config/systemd/user/voxtype-systray.service
systemctl --user daemon-reload
systemctl --user enable --now voxtype-systray.service
```

Note : cette méthode pose les fichiers hors gestion pacman. Préférer l'install via paquet pour une machine de production.

## Architecture

- **Multi-mode** : `main()` lit `argv[1]` et dispatch vers le mode tray (tokio) ou le mode settings (egui sur thread principal)
- **Crate ksni** : implémentation du protocole StatusNotifierItem (SNI) pour KDE/freedesktop
- **Canal dictée** : `voxtype status --format json --follow` (flux JSON ligne-à-ligne, push)
- **Canal réunion** : `voxtype meeting status` polled toutes les 2 s
- **Fusion** : l'état affiché est la combinaison des deux canaux. Rouge = daemon down (canal dictée uniquement). Vert = l'un OU l'autre actif.
- **Fallback** : `systemctl --user is-active voxtype` pour détecter l'arrêt du daemon
- **Robustesse** : pipe coupé, JSON malformé, daemon absent → dégradation gracieuse + retry toutes les 2 s
- **Icônes** : 3 PNG embarqués via `include_bytes!`, convertis RGBA → ARGB32 au démarrage
- **Menu dynamique** : `menu()` reconstruit les items à chaque ouverture depuis `self.state` (US-01). Actions non-bloquantes via `std::thread::spawn` + `std::process::Command`. Titre de réunion horodaté via `chrono::Local::now()`.
- **Réglages** : `eframe`/`egui` + `toml_edit` (édition TOML non-destructive) + `rfd` (dialogue de dossier XDG)
