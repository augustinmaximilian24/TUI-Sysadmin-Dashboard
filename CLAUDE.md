# Projekt: `logsentry` – Lokales Sysadmin-Dashboard mit Log-Anomalie-Erkennung

## 1. Ziel

Ein ressourcenschonendes Dashboard in Rust, das den systemd-Journal-Stream in Echtzeit
liest, Log-Zeilen zu Templates normalisiert und mit rein statistischen Verfahren
(Entropie, robuster Z-Score, Surprisal) Auffälligkeiten meldet – **und aus dem dem
sich direkt auf Probleme reagieren lässt**.

Kein Cloud-Dienst, keine LLM-Calls im Hot Path, keine Datenbank-Server.

Zielsysteme: Linux Mint Desktop (primär, grafische Oberfläche) und ein kleiner
Heimserver (Haswell-Klasse, headless, später per TUI oder SSH).

## 2. Scope

**In Scope (v1.0)**
- Ingestion von `journalctl -f -o json` (asynchron, non-blocking)
- Template-Extraktion (Regex-Masking + einfaches Drain-artiges Clustering)
- Anomalie-Scoring: Shannon-Entropie im Gleitfenster, robuster Z-Score (Median/MAD), Surprisal für neue Templates
- Systemzustand: CPU, RAM, Load, Disk, Temperaturen, Status ausgewählter systemd-Units
- Grafische Oberfläche (egui/eframe): Metriken, Live-Graph, Anomalie-Liste, Detail-Panel
- **Interaktives Aktions-Subsystem**: Unit neu starten, Prozess beenden, IP sperren, Anomalie stummschalten – jeweils mit Bestätigung
- Daemon-/Client-Trennung: Collector als systemd-Unit, GUI verbindet sich per Unix-Socket
- Replay-Modus für historische Logs (`--since`) zum Testen ohne Warten
- Persistenz der Baselines über Neustarts hinweg

**Out of Scope (v1.0)**
- Automatische Reaktion ohne Bestätigung (Auto-Remediation) – erst v2, mit Rate-Limit und Circuit-Breaker
- TUI-Client – das Protokoll wird dafür vorbereitet, gebaut wird er später
- Web-UI, Multi-Host-Aggregation, Alerting per Mail/Matrix
- ML-Modelle (Isolation Forest, TF-IDF)
- Windows-/macOS-Support
- LLM-Integration außer als explizit ausgelöste „Erkläre diese Anomalie"-Funktion

## 3. Tech-Stack

| Bereich | Crate / Technologie |
|---|---|
| Async-Runtime | `tokio` (process, sync::mpsc, net::UnixListener) |
| GUI | `eframe` / `egui`, Graphen mit `egui_plot` |
| Journal | `journalctl` als Subprozess, NDJSON via `tokio::io::BufReader` |
| Systemmetriken | `sysinfo` |
| systemd-Status & -Aktionen | `zbus` (org.freedesktop.systemd1), Autorisierung über polkit |
| Serialisierung | `serde`, `serde_json` |
| Persistenz | `redb` oder SQLite via `rusqlite` (Entscheidung in Phase 4 begründen) |
| Logging (eigenes) | `tracing` + `tracing-subscriber` |
| Fehler | `anyhow` (Binary), `thiserror` (Bibliotheksteile) |

Keine Abhängigkeit aufnehmen, ohne sie zu begründen. `ndarray` und `dashmap` sind
für diesen Umfang **nicht** nötig.

## 4. Architektur

```
journalctl -f -o json
        │  (NDJSON, eine Zeile = ein Event)
        ▼
[ Ingestion Task ]  tokio, eigener Task, mpsc::channel(1024), bounded
        ▼
[ Normalizer ]      Regex-Masking → Template-ID (Hash)
        ▼
[ Analysis Core ]   Ringpuffer 60 s · Entropie · Median/MAD-Z-Score · Surprisal
        ▼
[ State Store ]     aktueller Snapshot + Baselines (persistiert)
        │
        ├── Unix-Socket (JSON-Lines, bidirektional) ──► GUI-Client (egui)
        │        ▲  Snapshots raus                          │
        │        └─ Aktions-Requests rein ───────────────────┘
        │
[ Action Executor ] Allow-List aus Config · Audit-Log · Rate-Limit
        │
        ├── systemd D-Bus (RestartUnit, StopUnit …) über polkit
        ├── Signale an Prozesse (SIGTERM → SIGKILL)
        └── nftables-Set mit Ablaufzeit (IP-Sperre)
```

Der Collector läuft als privilegierter Daemon, die GUI ist ein reiner Client unter
dem Benutzerkonto. Diese Trennung ist verpflichtend.

## 5. Regeln für Claude

**Arbeitsweise**
1. Vor jeder Code-Änderung kurz den Plan nennen (max. 5 Zeilen), dann umsetzen.
2. Keine Phase überspringen. Erst wenn eine Phase kompiliert, getestet und committed ist, die nächste beginnen.
3. Nach jeder Änderung: `cargo check`, dann `cargo clippy -- -D warnings`, dann `cargo test`.
4. Keine erfundenen Crate-APIs. Bei Unsicherheit über eine Signatur: `cargo doc`/`cargo add` prüfen oder in der Doku nachsehen, nicht raten.
5. Compiler-Fehler nicht durch Deaktivieren von Lints, `unwrap()`-Ketten oder `#[allow(...)]` umgehen. Ursache beheben.
6. Änderungen klein halten: ein Commit = ein abgeschlossener Gedanke, Commit-Message auf Deutsch, Imperativ.

**Sicherheit und Rechte (nicht verhandelbar)**
7. Die GUI läuft **niemals** als Root. Kein `pkexec` auf das eigene Binary, kein Passwort durch die Anwendung reichen.
8. Der Client sendet ausschließlich **Aktions-IDs mit typisierten Parametern**, niemals Kommandostrings. Es gibt keine Codepfad, in dem Client-Eingaben in eine Shell gelangen.
9. Ausführbare Aktionen stehen als Allow-List in der Konfiguration. Was nicht in der Liste steht, wird abgelehnt und protokolliert.
10. systemd-Aktionen laufen über D-Bus mit polkit-Autorisierung, nicht über `Command::new("systemctl")`.
11. Der Socket liegt unter `/run/logsentry/`, gehört einer eigenen Gruppe und hat Modus 0660.
12. Jede Aktion braucht im UI einen Bestätigungsdialog mit Vorschau dessen, was ausgeführt wird.
13. Jede ausgeführte Aktion wird in ein Audit-Log geschrieben (Zeitpunkt, Benutzer, Aktion, Ergebnis) **und aus der eigenen Anomalie-Erkennung ausgefiltert**. Rückkopplung – Aktion erzeugt Logs, die eine Anomalie auslösen, die zur nächsten Aktion führt – muss durch einen expliziten Selbstfilter ausgeschlossen sein.
14. Rate-Limit pro Aktionstyp (z. B. max. 3 Neustarts derselben Unit pro 10 Minuten), danach Sperre mit Hinweis im UI.

**Code-Regeln**
15. Kein `unwrap()`/`expect()` außerhalb von Tests und `main()`-Initialisierung.
16. Kein `panic!` im Analysepfad. Fehlerhafte Log-Zeilen werden gezählt und verworfen, nicht als Absturz behandelt.
17. Alle Kanäle bounded. Bei Überlauf: ältestes Event verwerfen und Drop-Counter erhöhen (sichtbar im UI).
18. Alle Puffer haben eine harte Obergrenze. Keine unbeschränkt wachsende `VecDeque`/`Vec`.
19. Kein `MESSAGE`-Kurzschluss: das Journal liefert `MESSAGE` bei nicht-UTF-8-Inhalt als Byte-Array, nicht als String. Beide Fälle behandeln.
20. `eframe` im **reaktiven** Modus betreiben (Repaint nur bei Events bzw. `request_repaint_after`), nicht im Continuous-Modus. Zielrate 4–10 Aktualisierungen pro Sekunde.
21. Die GUI blockiert nie auf I/O. Socket-Kommunikation läuft in einem eigenen Task, die UI liest nur einen geteilten Snapshot.
22. Analyseparameter (Fenstergröße, Schwellwerte, Regex-Masken, Aktions-Allow-List) gehören in eine TOML-Konfigurationsdatei, nicht als Magic Numbers in den Code.
23. Öffentliche Funktionen mit Doc-Kommentar auf Deutsch. Inline-Kommentare nur, wo das „Warum" nicht offensichtlich ist.

**Qualität**
24. Jede Analysefunktion (Entropie, Z-Score, Masking, Template-Matching) bekommt Unit-Tests mit festen Fixtures.
25. Testdaten als Dateien unter `tests/fixtures/`, keine Live-Journal-Abhängigkeit in Tests.
26. Das Aktions-Subsystem bekommt einen Dry-Run-Modus, der ausschließlich protokolliert. Tests laufen nur im Dry-Run.
27. Idle-Last messen und dokumentieren. Zielwert: < 1 % CPU für den Daemon, < 2 % für die GUI im Leerlauf.
28. Kein Feature gilt als fertig, ohne dass es im Replay-Modus gegen echte historische Logs gelaufen ist.

**Kosten und Modellwahl**

29. Modellzuordnung nach Aufgabentyp:

| Modell | Zuständig für |
|---|---|
| **Sonnet 5** (Standard) | Boilerplate, Widgets, Regex-Masken, Unit-Tests, Doku, README, Commit-Messages, Formatierung, einfache Compilerfehler, Refactoring ohne Designänderung |
| **Opus 5** | Mehrdateiige Änderungen, Debugging über Modulgrenzen, Nebenläufigkeitsprobleme, Review von Analyse-Mathematik, API-Design innerhalb eines Crates |
| **Fable 5.1** | Nur: Protokoll-Design (Phase 6), Rechte- und Aktionsmodell (Phase 8), Baseline-/Persistenzschema (Phase 4) sowie Bugs, die nach zwei ernsthaften Versuchen mit Opus offen sind |

30. Phasen-Voreinstellung: Phasen 0, 1, 2, 5, 7 und 9 laufen mit Sonnet 5. Phasen 3 und 10 mit Opus 5. Phasen 4, 6 und 8 beginnen mit Fable 5.1 für den Entwurf und wechseln für die Umsetzung zurück auf Sonnet 5.
31. Zu Beginn jeder Phase und bei jeder Eskalation gibt Claude eine Zeile aus: `Modellempfehlung: <Modell> – Grund: <…>`. Das Umschalten selbst erfolgt manuell, Claude wartet die Bestätigung ab.
32. Eskalationsregel: Erst nach zwei gescheiterten Lösungsversuchen auf derselben Ebene wird ein Modell höher gegangen – und mit einer verdichteten Problembeschreibung, nicht mit dem gesamten Verlauf. Nach dem gelösten Problem wird sofort wieder heruntergestuft.
33. Fable 5.1 wird nie für Tests, Doku, Formatierung oder Commit-Messages verwendet, auch nicht „weil es gerade eingestellt ist".
34. Keine kompletten Log-Dateien oder `target/`-Ausgaben in den Kontext laden. Immer nur den relevanten Ausschnitt.
35. Bei Kontextwechsel zwischen Phasen den Verlauf leeren und aus dieser Beschreibung neu starten.
36. Budget: rund 100 $ Gesamtguthaben. Nach jeder abgeschlossenen Phase kurz den bisherigen Verbrauch prüfen. Wenn nach Phase 5 mehr als die Hälfte verbraucht ist, werden die Phasen 8 und 10 auf das Nötigste gekürzt, nicht die Sicherheitsregeln.

**Modellempfehlung je Phase**

| Phase | Modell | Begründung |
|---|---|---|
| 0 – Gerüst | Sonnet 5 | Workspace, TOML-Loading, leeres Fenster. Reine Mechanik. |
| 1 – Ingestion | Sonnet 5 | Subprozess, NDJSON, Backoff. Gut dokumentierte Standardaufgaben. |
| 2 – Normalisierung | Sonnet 5 | Regex-Masken und Hashing. Einzige Ausnahme: das Drain-Clustering – wenn es hakt, auf Opus 5 eskalieren. |
| 3 – Analyse | Opus 5 | Entropie, Median/MAD, Surprisal, Score-Kombination mit Hysterese. Hier entscheidet sich die Fehlalarmquote. |
| 4 – Baselines & Persistenz | Fable 5.1 → Sonnet 5 | Schema und Migrationspfad einmal richtig entwerfen, danach Umsetzung mit Sonnet. |
| 5 – Systemzustand | Sonnet 5 | sysinfo und zbus-Abfragen. Ablesen, nicht entscheiden. |
| 6 – Daemon & Protokoll | Fable 5.1 → Sonnet 5 | Das Protokoll trägt später GUI, TUI und Aktionen. Ein Fehlentwurf hier kostet dich alle drei. |
| 7 – GUI | Sonnet 5 | egui-Widgets und Layout. Viel Code, wenig Entscheidung. |
| 8 – Aktions-Subsystem | Fable 5.1 → Sonnet 5 | Rechtemodell, Allow-List, Selbstfilter gegen Rückkopplung. Der sicherheitskritischste Teil. |
| 9 – Auslieferung | Sonnet 5 | systemd-Unit, .desktop, README, Installationsskript. |
| 10 – Optional | Opus 5 | TUI-Client und Exporte. Mehrdateiig, aber auf bestehendem Protokoll. |

## 6. Aufgaben / Phasen

**Phase 0 – Gerüst**
- [x] `cargo new logsentry`, Workspace-Struktur (`core/`, `daemon/`, `gui/`, `proto/`)
- [x] `rustfmt.toml`, `clippy.toml`, `justfile` oder `Makefile`
- [x] Konfigurations-Struct + TOML-Laden, Default-Konfiguration
- [x] Minimales eframe-Fenster, das startet und sauber schließt

**Phase 1 – Ingestion**
- [x] `journalctl -f -o json --no-pager -n 0` als Tokio-Subprozess, zeilenweise lesen
- [x] Deserialisierung der relevanten Felder (`__REALTIME_TIMESTAMP`, `_SYSTEMD_UNIT`, `_PID`, `PRIORITY`, `MESSAGE`, `_HOSTNAME`)
- [x] Byte-Array-Variante von `MESSAGE` behandeln
- [x] Rechteprüfung beim Start: Ist der Benutzer in Gruppe `systemd-journal`/`adm`? Sonst klare Fehlermeldung mit Lösungsvorschlag
- [x] Prozessabbruch erkennen und mit Backoff neu starten
- [x] Replay-Modus: `--since`/`--until` statt `-f`

**Phase 2 – Normalisierung**
- [x] Regex-Masken für Zeitstempel, PIDs, IPv4/IPv6, MAC, UUIDs, Pfade, Hex-Adressen, Zahlen
- [x] Template-ID über stabilen Hash, Template-Registry mit Erstsichtungs-Zeitpunkt
- [x] Einfaches Drain-artiges Clustering für Zeilen, die die Masken nicht abdecken
- [x] Tests: 30 reale Beispielzeilen → erwartete Templates

**Phase 3 – Analyse**
- [x] Ringpuffer über konfigurierbares Zeitfenster (Default 60 s)
- [x] Shannon-Entropie über die Template-Verteilung im Fenster
- [x] Robuster Z-Score über Median/MAD statt Mittelwert/σ (Log-Raten sind nicht normalverteilt)
- [x] Surprisal `-log2 P(template)` → hoher Score für erstmals gesehene Templates
- [x] Score-Kombination zu einem Anomalie-Level (info/warn/critical) mit Hysterese
- [x] Dedup + Cooldown, damit dieselbe Anomalie nicht 200× erscheint
- [x] Lernphase: erste N Minuten nur beobachten, nicht alarmieren

**Phase 4 – Baselines & Persistenz**
- [x] Baselines pro Unit statt global
- [x] Tageszeit-/Wochentagsprofil (nachts andere Normalität als Montag früh)
- [x] Persistenz über Neustart, Migrationspfad für das Schema

**Phase 5 – Systemzustand**
- [x] `sysinfo`: CPU, RAM, Load, Disk, Temperaturen
- [x] `zbus`: Status ausgewählter systemd-Units (`ListUnits`, `ActiveState`, `SubState`)
- [x] GPU-Temperatur nur, wenn eine Quelle vorhanden ist (hwmon bzw. NVML) – sonst Feld ausblenden statt raten

**Phase 6 – Daemon & Protokoll**
- [x] Collector-Binary mit Unix-Socket-Server unter `/run/logsentry/`
- [x] Protokoll in eigenem Crate `proto/`: Snapshot-Nachrichten und Aktions-Requests als versionierte Enums
- [x] Mehrere gleichzeitige Clients, Reconnect-Verhalten definiert
- [x] Socket-Rechte über eigene Gruppe, Zugriffsverweigerung sauber melden

**Phase 7 – GUI**
- [x] Kopfbereich: Host, Uptime, Entropie, Drop-Counter, Lernphasen-Status, Verbindungsstatus
- [x] Live-Graph mit `egui_plot` und **dynamischer** Y-Achse (keine feste Obergrenze)
- [x] Anomalie-Liste, sortier- und filterbar nach Unit, Priorität und Score
- [x] Detail-Panel: Rohzeilen um die Anomalie herum, betroffene Unit und PID, Score-Aufschlüsselung
- [x] Systemzustands-Panel mit Unit-Status
- [x] Pause-Funktion, Suchfeld, Hell/Dunkel-Umschaltung
- [x] Reaktiver Repaint-Modus verifiziert (Leerlauf-Last gemessen)

**Phase 8 – Aktions-Subsystem**
- [x] `ActionRequest`-Enum im Protokoll, Allow-List in der Konfiguration
- [x] Executor im Daemon mit Dry-Run-Schalter, Audit-Log und Rate-Limit
- [x] Selbstfilter: eigene Aktions-Logs fließen nicht in die Anomalie-Erkennung
- [x] Aktion: Unit neu starten / stoppen (D-Bus + polkit), Ergebnis live im UI verfolgen
- [x] Aktion: Prozess beenden (SIGTERM, nach Timeout SIGKILL)
- [x] Aktion: IP temporär sperren über `nftables`-Set mit Ablaufzeit
- [x] Aktion: Anomalie stummschalten (1 h / 1 Tag / dauerhaft in Baseline übernehmen)
- [x] Aktion: Journal-Ausschnitt exportieren bzw. in die Zwischenablage
- [x] Bestätigungsdialog mit Vorschau für jede Aktion, kein Ein-Klick-Vollzug
- [x] Audit-Ansicht im UI: was wurde wann von wem ausgelöst

**Phase 9 – Auslieferung**
- [x] systemd-Unit für den Daemon mit `Nice=10`, `IOSchedulingClass=idle`, `MemoryMax=`, `ProtectSystem=strict`, `NoNewPrivileges=yes`
- [x] `.desktop`-Datei für die GUI
- [x] Installationsskript, Gruppenanlage, README mit Screenshot
- [ ] Idle-Last auf dem Heimserver gemessen und dokumentiert -- von dieser Maschine (Desktop) aus nicht möglich, steht noch aus

**Phase 10 – Optional**
- [ ] TUI-Client als zweiter Konsument desselben Protokolls (für headless/SSH)
- [ ] Prometheus-Textfile-Export
- [ ] Tages-Export als JSON/CSV zur Weiterverarbeitung
- [ ] „Erkläre diese Anomalie": ausgewählte Zeilen + Kontext auf Knopfdruck an ein LLM, Ergebnis im Detail-Panel

**Phase 11 – Wissensgraph-Tab (optional, außerhalb des ursprünglichen v1.0-Scopes)**

Zeigt den von einem separaten Tool (`graphify`) erzeugten persönlichen
Wissensgraphen (`~/.claude/activity-log/graphify-out/graph.json`) direkt
als Tab in der GUI, statt dass dafür `graph.html` im Browser geöffnet
werden muss. `graph.json` selbst ist kein logsentry-Datenmodell -- dieser
Tab ist ein reiner Konsument einer externen, optionalen Datei.

- [x] `KnowledgeGraphConfig` (`core/src/config.rs`): Pfad, Poll-Intervall,
      Rotationsgeschwindigkeit, Idle-Timeout, Drag-Sensitivität, Layout-
      Iterationen -- alles konfigurierbar statt Magic Numbers (Regel 22)
- [x] Parser für `graph.json` (`gui/src/knowledge_graph/data.rs`)
- [x] Deterministisches 3D-Force-Layout, da `graph.json` keine Positionen
      enthält (`gui/src/knowledge_graph/layout.rs`)
- [x] Reine Rotations-/Projektionsmathematik, ohne `egui`-Abhängigkeit
      testbar (`gui/src/knowledge_graph/camera.rs`)
- [x] Hintergrund-Thread pollt `graph_json_path` per `mtime` und lädt bei
      Änderung (z. B. durch `graphify --update`) automatisch neu (Regel 21)
- [x] Rendering per `egui::Painter`: Community-Farben, gradbasierte
      Knotengröße, Hyperkanten als konvexe Hülle mit Label (wie in
      graphify's `graph.html`)
- [x] Maus-Drag dreht frei (Yaw/Pitch), nach `idle_resume_secs` ohne
      Interaktion übernimmt wieder die automatische horizontale Rotation;
      Scroll zoomt
- [x] Klick auf einen Knoten zeigt Label, Community, Typ, Quelle,
      Rationale und Verbindungen (Relation + Gegenknoten) in einem
      Overlay
- [x] Tab-Umschalter im Header (Dashboard/Wissensgraph), Tab nur
      sichtbar wenn `knowledge_graph.enabled = true`
- [x] Visuell verifiziert: Tab rendert Knoten/Kanten/Hyperkanten korrekt
      und die automatische Rotation läuft sichtbar (Screenshot-Vergleich
      über zwei Zeitpunkte). Interaktive Maus-Drag-/Klick-Verifikation
      war in der Entwicklungsumgebung nicht möglich (`xdotool`-Synthetic-
      Events erreichten das winit-Fenster nicht) -- die Interaktionslogik
      selbst folgt denselben `egui::Sense`/`Response`-Mustern wie die
      bereits funktionierenden Buttons in `app.rs` und ist über
      `camera::apply_drag`/`project_point` auf Ebene der reinen
      Mathematik unit-getestet.

## 7. Definition of Done (v1.0)

- Daemon läuft 72 h ohne Absturz und ohne Speicherwachstum
- Idle-CPU-Last: Daemon unter 1 %, GUI unter 2 %; RSS des Daemons unter 50 MB
- Ein simulierter SSH-Bruteforce und ein OOM-Kill werden im Replay zuverlässig erkannt
- Weniger als 5 Fehlalarme pro Tag im Normalbetrieb
- Jede Aktion aus dem UI funktioniert, ist im Audit-Log nachvollziehbar und löst keine Folge-Anomalie aus
- Kein Codepfad, in dem Client-Eingaben in eine Shell oder in Root-Rechte gelangen
- `cargo clippy -- -D warnings` und `cargo test` laufen sauber durch
- README erklärt Installation, Konfiguration, Rechtevergabe und Aktionen
