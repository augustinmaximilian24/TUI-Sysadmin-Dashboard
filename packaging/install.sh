#!/bin/bash
# Installiert logsentry aus einem bereits gebauten Release-Build
# (`cargo build --release`) systemweit. Muss als root laufen, weil es
# Binaries nach /usr/local/bin kopiert, eine Systemgruppe anlegt und die
# systemd-Unit installiert.
#
# Aufruf: sudo ./packaging/install.sh    (aus dem Projekt-Wurzelverzeichnis)
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "Bitte als root ausführen: sudo ./packaging/install.sh" >&2
    exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR=/usr/local/bin
CONFIG_DIR=/etc/logsentry

DAEMON_BIN="$REPO_ROOT/target/release/logsentry-daemon"
GUI_BIN="$REPO_ROOT/target/release/logsentry-gui"

if [ ! -x "$DAEMON_BIN" ] || [ ! -x "$GUI_BIN" ]; then
    echo "Release-Binaries fehlen. Erst bauen: cargo build --release" >&2
    exit 1
fi

echo "Lege Systemgruppe 'logsentry' an (falls nicht vorhanden) ..."
groupadd --system logsentry 2>/dev/null || true

echo "Kopiere Binaries nach $BIN_DIR ..."
install -Dm755 "$DAEMON_BIN" "$BIN_DIR/logsentry-daemon"
install -Dm755 "$GUI_BIN" "$BIN_DIR/logsentry-gui"

if [ ! -f "$CONFIG_DIR/logsentry.toml" ]; then
    echo "Installiere Beispiel-Konfiguration nach $CONFIG_DIR/logsentry.toml ..."
    install -Dm644 "$REPO_ROOT/packaging/logsentry.toml.example" "$CONFIG_DIR/logsentry.toml"
else
    echo "$CONFIG_DIR/logsentry.toml existiert bereits, wird nicht überschrieben."
fi

echo "Installiere systemd-Unit ..."
install -Dm644 "$REPO_ROOT/packaging/systemd/logsentry.service" /etc/systemd/system/logsentry.service
systemctl daemon-reload

echo "Installiere .desktop-Datei für die GUI ..."
install -Dm644 "$REPO_ROOT/packaging/logsentry.desktop" /usr/share/applications/logsentry.desktop

cat <<'EOF'

Fertig. Nächste Schritte:

1. Konfiguration prüfen, insbesondere den Abschnitt [actions]:
     sudo nano /etc/logsentry/logsentry.toml

2. Den eigenen Benutzer für die GUI in die Gruppe 'logsentry' aufnehmen
   (danach einmal ab-/anmelden, damit die Gruppenmitgliedschaft greift):
     sudo usermod -aG logsentry "$USER"

3. Daemon aktivieren und starten:
     sudo systemctl enable --now logsentry.service
     systemctl status logsentry.service

4. GUI starten (aus dem Anwendungsmenü oder direkt):
     logsentry-gui
EOF
