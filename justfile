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
