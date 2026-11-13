# Entwicklungs-Workflow für logsentry.
# Aufruf: just <recipe>, z. B. `just check-all`

# Formatierung prüfen (schreibt nicht).
fmt-check:
    cargo fmt --all -- --check

# Formatierung anwenden.
fmt:
    cargo fmt --all

# Kompilierbarkeit prüfen.
check:
    cargo check --workspace --all-targets

# Lints ohne Warnungen (Regel 3/5: keine #[allow] zum Umgehen).
clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# Tests ausführen.
test:
    cargo test --workspace

# Kompletter Zyklus, wie in Regel 3 vorgeschrieben.
check-all: check clippy test

# Daemon lokal starten (Ingestion folgt in Phase 1).
run-daemon:
    cargo run -p logsentry-daemon

# GUI lokal starten.
run-gui:
    cargo run -p logsentry-gui

# 3D-Werkbank-Prototyp im Browser öffnen (offline, ohne Daemon).
werkbank-vorschau:
    xdg-open prototypes/werkbank-3d/index.html

# 3D-Werkbank als eigenes Programmfenster starten (GTK + WebKitGTK, kein Browser).
# Baut vorher die GUI: ihr Modus --netzkarte-json liefert die Daten der Weltkugel.
werkbank:
    cargo build --release -p logsentry-gui
    python3 prototypes/werkbank-3d/werkbank.py

# Werkbank ins Startmenü eintragen (nur für den eigenen Benutzer, ohne sudo).
werkbank-installieren:
    sed "s|@WERKBANK_DIR@|{{justfile_directory()}}/prototypes/werkbank-3d|" prototypes/werkbank-3d/logsentry-werkbank.desktop > ~/.local/share/applications/logsentry-werkbank.desktop
    update-desktop-database ~/.local/share/applications 2>/dev/null || true
    @echo "Eingetragen: Startmenü → logsentry Werkbank"
