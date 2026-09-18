# logsentry installieren (von USB-Stick) — manuelle Installation

logsentry ist ein lokales Sysadmin-Dashboard: ein privilegierter Daemon liest
das systemd-Journal und erkennt Anomalien statistisch, eine grafische
Oberfläche (läuft nie als root) zeigt sie an.

> Es gibt auch eine **Schnellinstallation per Doppelklick**
> (`logsentry-installieren.sh` / `logsentry-installieren.desktop`),
> siehe `logsentry-ANLEITUNG-AUTOMATISCH.md`. Diese hier ist der manuelle
> Weg — nützlich als Referenz, wenn man genau wissen will, was passiert,
> oder falls die automatische Installation aus irgendeinem Grund nicht
> funktioniert.

## Voraussetzungen auf dem Ziel-PC

- Linux, x86_64 (64-Bit), systemd
- glibc **2.39 oder neuer** (z. B. Linux Mint 22, Ubuntu 24.04, Debian 13
  oder neuer). Auf älteren Distros (Ubuntu 22.04, Debian 12, Mint 21) läuft
  es **nicht** ohne Neubau aus dem Quellcode.
- Für die GUI: eine normale grafische Desktop-Umgebung (X11 oder Wayland) —
  auf jedem üblichen Desktop-Linux von Haus aus vorhanden.
- `sudo`-Rechte für die Installation.

## Installation

1. Diesen Ordner vom Stick auf die Festplatte kopieren (nicht direkt vom
   Stick installieren) und hineinwechseln, z. B.:

   ```bash
   cp -r /media/*/*/logsentry-0.1.0-linux-x86_64 ~/logsentry-install
   cd ~/logsentry-install
   ```

2. Installations-Skript als root ausführen:

   ```bash
   sudo ./packaging/install.sh
   ```

   Das Skript legt die Systemgruppe `logsentry` an, kopiert die Binaries
   nach `/usr/local/bin`, installiert eine Beispiel-Konfiguration nach
   `/etc/logsentry/logsentry.toml`, richtet den systemd-Service ein und
   installiert den Menüeintrag für die GUI.

3. Eigenen Benutzer in die Gruppe `logsentry` aufnehmen, damit die GUI sich
   mit dem Daemon verbinden darf, danach einmal ab- und wieder anmelden:

   ```bash
   sudo usermod -aG logsentry "$USER"
   ```

4. Daemon aktivieren und starten:

   ```bash
   sudo systemctl enable --now logsentry.service
   systemctl status logsentry.service
   ```

5. GUI starten (aus dem Anwendungsmenü unter "logsentry" oder direkt im
   Terminal):

   ```bash
   logsentry-gui
   ```

## Wichtig: dry_run

In `/etc/logsentry/logsentry.toml` steht standardmäßig `dry_run = true` —
der Daemon protokolliert mögliche Aktionen (z. B. Prozess beenden, Unit
neu starten) nur, führt sie aber **nicht aus**. Das sollte man nur bewusst
auf `false` stellen, nachdem man die Konfiguration (`[actions]`-Abschnitt,
`allowed_kinds`, `allowed_units`) geprüft hat.

## Enthalten in diesem Paket

- `target/release/logsentry-daemon`, `logsentry-gui` — fertig gebaute
  Binaries (x86_64, glibc 2.39+)
- `packaging/install.sh` — Installationsskript
- `packaging/systemd/logsentry.service` — systemd-Unit
- `packaging/logsentry.toml.example` — Beispiel-Konfiguration
- `packaging/logsentry.desktop` — Menüeintrag für die GUI

Quelltext und weitere Doku: siehe GitHub-Repo `augustinmaximilian24/TUI-Sysadmin-Dashboard`.
