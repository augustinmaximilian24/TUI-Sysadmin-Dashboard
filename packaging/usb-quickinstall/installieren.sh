#!/bin/bash
# logsentry Schnellinstallation
#
# Diese Datei ist ein selbstentpackendes Installationsskript: das komplette
# Programmpaket (Binaries + Konfiguration) ist als Base64-Text an dieses
# Skript angehaengt. Einfach ausfuehren (Doppelklick -> "Ausfuehren", oder
# im Terminal: bash logsentry-installieren.sh) -- der Rest passiert
# automatisch.
#
# Bei Problemen: siehe logsentry-ANLEITUNG-AUTOMATISCH.md auf dem Stick,
# Abschnitt "Problembehandlung". Alternativ die manuelle Installation
# gemaess logsentry-ANLEITUNG-MANUELL.md.

set -euo pipefail

log() { echo "[logsentry-install] $*"; }
err() { echo "[logsentry-install] FEHLER: $*" >&2; }

SELF="$(readlink -f "$0")"

# --- Root-Rechte sicherstellen ---------------------------------------
if [ "$(id -u)" -ne 0 ]; then
    if command -v pkexec >/dev/null 2>&1; then
        log "Starte mit Administratorrechten (pkexec) neu ..."
        exec pkexec bash "$SELF" "$@"
    elif command -v sudo >/dev/null 2>&1; then
        log "Starte mit Administratorrechten (sudo) neu ..."
        exec sudo bash "$SELF" "$@"
    else
        err "Weder 'pkexec' noch 'sudo' gefunden -- kann keine Root-Rechte anfordern."
        err "Bitte manuell als root ausfuehren, z. B.: su -c 'bash \"$SELF\"'"
        err "Oder die manuelle Installation verwenden (logsentry-ANLEITUNG-MANUELL.md)."
        exit 1
    fi
fi

# --- Aufrufenden (nicht-root) Benutzer ermitteln ----------------------
REAL_USER="${SUDO_USER:-}"
if [ -z "$REAL_USER" ] && [ -n "${PKEXEC_UID:-}" ]; then
    REAL_USER="$(id -nu "$PKEXEC_UID" 2>/dev/null || true)"
fi
if [ -z "$REAL_USER" ]; then
    REAL_USER="$(logname 2>/dev/null || true)"
fi

# --- Payload finden und entpacken -------------------------------------
PAYLOAD_LINE="$(grep -an '^__PAYLOAD_BELOW__$' "$SELF" | head -1 | cut -d: -f1)"
if [ -z "$PAYLOAD_LINE" ]; then
    err "Konnte die angehaengten Programmdateien in dieser Datei nicht finden."
    err "Die Datei ist vermutlich beschaedigt oder unvollstaendig kopiert worden."
    exit 1
fi
PAYLOAD_START=$((PAYLOAD_LINE + 1))

WORKDIR="$(mktemp -d /tmp/logsentry-install.XXXXXX)"
trap 'rm -rf "$WORKDIR"' EXIT

log "Entpacke Programmdateien ..."
if ! tail -n +"$PAYLOAD_START" "$SELF" | base64 -d | tar -xz -C "$WORKDIR"; then
    err "Entpacken fehlgeschlagen -- die Datei ist vermutlich beschaedigt."
    err "Bitte die Datei nochmal frisch vom Original-Stick kopieren."
    exit 1
fi

STAGE="$(find "$WORKDIR" -maxdepth 1 -mindepth 1 -type d | head -1)"
if [ -z "$STAGE" ] || [ ! -x "$STAGE/packaging/install.sh" ]; then
    err "Entpacktes Paket ist unvollstaendig (packaging/install.sh fehlt)."
    exit 1
fi

# --- Eigentliche Installation ------------------------------------------
log "Installiere logsentry (Daemon, GUI, systemd-Service) ..."
bash "$STAGE/packaging/install.sh"

if [ -n "$REAL_USER" ]; then
    log "Fuege Benutzer '$REAL_USER' zur Gruppe 'logsentry' hinzu ..."
    usermod -aG logsentry "$REAL_USER" || err "usermod fehlgeschlagen, bitte manuell nachholen: sudo usermod -aG logsentry $REAL_USER"
else
    log "Konnte den aufrufenden Benutzer nicht automatisch ermitteln."
    log "Bitte manuell ausfuehren: sudo usermod -aG logsentry <dein-benutzername>"
fi

log "Aktiviere und starte den logsentry-Dienst ..."
if ! systemctl enable --now logsentry.service; then
    err "Dienst konnte nicht gestartet werden. Details mit:"
    err "  systemctl status logsentry.service"
    err "  journalctl -u logsentry.service --no-pager -n 50"
    exit 1
fi

cat <<EOF

=========================================================
 logsentry wurde installiert, der Dienst laeuft.

 WICHTIG: Damit die GUI sich mit dem Dienst verbinden
 darf, muss sich ${REAL_USER:-dein Benutzer} einmal ab-
 und wieder anmelden (die neue Gruppenmitgliedschaft
 'logsentry' wird sonst erst beim naechsten Login aktiv).

 Danach die GUI starten:
   - ueber das Anwendungsmenue ("logsentry"), oder
   - im Terminal mit: logsentry-gui
=========================================================
EOF

if [ -t 0 ]; then
    read -rp "Enter druecken zum Schliessen ... " _ || true
fi

exit 0
__PAYLOAD_BELOW__
