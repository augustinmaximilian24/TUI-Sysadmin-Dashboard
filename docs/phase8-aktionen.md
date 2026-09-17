# Phase 8 – Aktions-Subsystem: Entwurf

Stand: Entwurf (Ersatz für die in CLAUDE.md vorgesehene Fable-5.1-Design-Runde,
siehe README-Anmerkung zur Modellzuordnung). Umsetzung erfolgt mit Sonnet 5
nach diesem Dokument. Abschnitte 3–7 sind normativ.

## 1. Entscheidungen im Überblick

| Frage | Entscheidung | Begründung |
|---|---|---|
| Wo lebt die Allow-List? | Neue `[actions]`-Sektion in `logsentry.toml`, geladen in `ActionsConfig`. | Regel 9/22: Konfiguration, keine Magic Numbers/Strings im Code. |
| Default bei fehlender Konfiguration? | `dry_run = true`, `allowed_kinds = []`, `allowed_units = []`. | Fail-safe: ein frisch installierter Daemon führt nichts aus, bis der Betreiber explizit zustimmt. |
| Wie wird `RestartUnit`/`StopUnit` ausgeführt? | `zbus`-Aufruf von `org.freedesktop.systemd1.Manager.{RestartUnit,StopUnit}` auf dem System-Bus (derselbe Proxy-Ansatz wie `daemon::units` in Phase 5). | Regel 10: kein `Command::new("systemctl")`. Polkit-Autorisierung greift automatisch über die D-Bus-Policy des System-Bus. |
| Wie wird `TerminateProcess` ausgeführt? | `nix::sys::signal::kill` (SIGTERM), nach `grace_secs` per verzögertem Tokio-Task Existenzprüfung via erneutem `kill(pid, 0)` und ggf. SIGKILL. | Bereits vorhandene `nix`-Abhängigkeit, kein Subprozess nötig. |
| Wie wird `BlockIp` ausgeführt? | `std::process::Command::new("nft")` mit einzeln übergebenen, typisierten Argumenten (nie über eine Shell, nie String-interpoliert). | Regel 8 verbietet Kommandostrings *vom Client* in eine Shell -- hier ist die IP zum Zeitpunkt des Aufrufs bereits ein geprüftes `IpAddr`, die Dauer ein geklemmtes `u32`, beide werden als eigene `argv`-Elemente übergeben. `nft` hat (anders als systemd) keine für diesen Zweck geeignete D-Bus-API im Ökosystem; ein Netlink-Crate (`nftnl`) wäre die Alternative, ist aber unreif und für dieses Projektmaß nicht gerechtfertigt (Regel: keine Abhängigkeit ohne Begründung). |
| Wie wird `MuteAnomaly` ausgeführt? | Kein externer Seiteneffekt: trägt `(template_id, unit)` mit Ablaufzeit in eine neue `MuteStore`-Struktur in `SharedState` ein; die Analyse-Pipeline prüft das vor dem Melden. | Rein interner Zustand, keine Systemänderung -- braucht trotzdem Allow-List/Rate-Limit/Audit wie jede andere Aktion (Konsistenz). |
| Selbstfilter (Regel 13)? | `SharedState::self_filter`: `HashMap<SelfFilterKey, u64>` (Ablaufzeitpunkt in µs), Schlüssel `Unit(String)` oder `Pid(i32)`. `RestartUnit`/`StopUnit` tragen die Ziel-Unit für `self_filter_window_secs` ein, `TerminateProcess` die PID. `Pipeline::handle` prüft vor `engine.process(...)`, überspringt die Analyse bei Treffer, aber `push_context` bleibt unbedingt (Audit-Sichtbarkeit laut `docs/phase6-protokoll.md` Abschnitt 9). |
| Rate-Limit-Schlüssel? | `(ActionKind, Ziel-String)`, z. B. `("restart_unit", "sshd.service")` oder `("block_ip", "203.0.113.5")`. Gleitfenster über `VecDeque<u64>` (Zeitstempel der letzten Versuche), geprüft und bereinigt bei jedem Aufruf. | Regel 14 spricht explizit von "derselben Unit" -- der Schlüssel muss das Ziel einschließen, nicht nur die Aktionsart. |
| Wer zählt für das Rate-Limit? | Jeder *Versuch* (auch ein späterer `Denied`), nicht nur erfolgreiche Ausführungen. | Sonst kann ein Client das Limit durch absichtliches Scheitern umgehen. |
| Audit-Log-Format | Eine JSON-Zeile pro Versuch (JSONL) unter `actions.audit_log_path`, Default `/var/lib/logsentry/audit.jsonl`. Felder: Zeitstempel, `session_id` des anfragenden Clients, Aktionsart, Ziel, Ergebnis (inkl. `Denied`/`Failed`), `dry_run`. | Konsistent mit dem übrigen Persistenzformat (JSON), grep-/jq-fähig ohne zusätzliche Tools. |
| Bestätigungsdialog | Bleibt GUI-seitig (Phase 8 GUI-Teil, eigener Schritt): Button löst *nicht* direkt `ClientMessage::Action` aus, sondern öffnet ein Modal mit Vorschau; erst dessen Bestätigung sendet die Anfrage. | Regel 12. |

## 2. Neue Abhängigkeiten

Keine. `zbus` (Phase 5), `nix` (Phase 6) und der bereits im Basissystem vorhandene `nft`-Binärname decken alles ab.

## 3. Konfiguration (normativ)

```rust
#[serde(default)]
pub struct ActionsConfig {
    pub dry_run: bool,                       // Default: true
    pub allowed_kinds: Vec<String>,          // Default: [] -- Werte: "restart_unit", "stop_unit",
                                              // "terminate_process", "block_ip", "mute_anomaly"
    pub allowed_units: Vec<String>,          // Default: [] -- nur für restart_unit/stop_unit geprüft
    pub rate_limit_max_actions: u32,         // Default: 3
    pub rate_limit_window_minutes: u64,      // Default: 10
    pub self_filter_window_secs: u64,        // Default: 30
    pub terminate_default_grace_secs: u16,   // Default: 5
    pub terminate_max_grace_secs: u16,       // Default: 60
    pub block_ip_min_duration_secs: u32,     // Default: 60
    pub block_ip_max_duration_secs: u32,     // Default: 604_800 (7 Tage)
    pub nftables_family: String,             // Default: "inet"
    pub nftables_table: String,              // Default: "filter"
    pub nftables_set_v4: String,             // Default: "logsentry_blocked_v4"
    pub nftables_set_v6: String,             // Default: "logsentry_blocked_v6"
    pub audit_log_path: String,              // Default: "/var/lib/logsentry/audit.jsonl"
}
```

`allowed_kinds` als `Vec<String>` statt `Vec<ActionKind>`: eine unbekannte Zeichenkette in der TOML-Datei soll die Konfiguration nicht zum Scheitern bringen (Regel: fehlende/falsche Felder führen zu sinnvollen Defaults, nicht zum Absturz), sondern beim Allow-List-Abgleich einfach nie matchen. Das Mapping String → `ActionKind` liegt in `daemon::actions`.

## 4. Self-Filter

```rust
enum SelfFilterKey { Unit(String), Pid(i32) }
```

`SharedState` bekommt `self_filter: Mutex<HashMap<SelfFilterKey, u64>>` (Ablauf-Zeitstempel µs) mit `suppress_unit`, `suppress_pid`, `is_suppressed(unit, pid, now_us) -> bool` (bereinigt abgelaufene Einträge bei jedem Aufruf -- Regel 18, Wachstum ist ohnehin durch die Seltenheit von Aktionen begrenzt, aber abgelaufene Einträge sollen nicht ewig liegen bleiben).

`Pipeline::handle` ruft `is_suppressed` **vor** `engine.process(...)` auf; bei Treffer wird die Analyse übersprungen (Zähler `self_filtered` in `PipelineSummary`/`PipelineStats` für Transparenz in der GUI), aber `push_context` und `templates.process` (für Statistik) laufen unverändert weiter.

## 5. Rate-Limit

`ActionExecutor` hält `rate_limits: Mutex<HashMap<String, VecDeque<u64>>>`, Schlüssel `"{kind}:{target}"`. Bei jedem Versuch: Einträge älter als `rate_limit_window_minutes` verwerfen, dann Länge prüfen. `retry_after_secs` in `DenyReason::RateLimited` ist die Zeit bis der älteste Eintrag aus dem Fenster fällt.

## 6. Ausführung je Aktionsart

- **RestartUnit/StopUnit**: `UnitName` bereits protokollseitig geprüft (Zeichensatz + Suffix). Zusätzlich gegen `allowed_units` prüfen (exakter String-Vergleich, keine Muster/Wildcards -- Regel 9 will eine geschlossene Liste). D-Bus-Aufruf mit Modus `"replace"`. Selbstfilter auf die Ziel-Unit, `self_filter_window_secs`.
- **TerminateProcess**: `grace_secs` auf `[1, terminate_max_grace_secs]` klemmen. SIGTERM sofort, danach ein `tokio::spawn`, das nach `grace_secs` erneut prüft (`kill(pid, None)` als Existenztest) und bei Bedarf SIGKILL sendet. Selbstfilter auf die PID.
- **BlockIp**: `duration_secs` auf `[block_ip_min_duration_secs, block_ip_max_duration_secs]` klemmen. Idempotent: zuerst Tabelle/Set sicherstellen (`add table`/`add set`, Fehler "already exists" wird ignoriert), dann `add element ... { <ip> timeout <secs>s }`. Kein Selbstfilter nötig (erzeugt keine Journal-Zeilen der beobachteten Units).
- **MuteAnomaly**: Ablaufzeitpunkt aus `MuteScope` (`OneHour`/`OneDay`/`Permanent` = `u64::MAX`) berechnen, in `SharedState::mute_store` eintragen. Die Analyse-Engine (`AnalysisEngine::process` oder ein vorgelagerter Check in `Pipeline::handle`) unterdrückt gemutete `(template_id, unit)`-Paare wie eine dauerhafte Cooldown-Sperre.

Jede Ausführung -- auch Fehlschläge -- liefert eine aussagekräftige `message` in `ActionOutcome::{Completed,Failed}`, die die GUI unverändert anzeigen kann.

## 7. Daemon-Struktur

| Datei | Inhalt |
|---|---|
| `daemon/src/actions.rs` | `ActionExecutor`, Allow-List-/Rate-Limit-Prüfung, Dispatch je Aktionsart, Audit-Log. |
| `daemon/src/state.rs` (Erweiterung) | `self_filter`, `mute_store`. |
| `daemon/src/client_task.rs` (Änderung) | `ClientMessage::Action` ruft `ActionExecutor::execute` statt der Phase-6-Pauschalablehnung; `Hello.allowed_actions`/`dry_run` aus der Konfiguration statt hartkodiert. |
| `daemon/src/pipeline.rs` (Änderung) | Selbstfilter- und Mute-Check vor `engine.process`. |

## 8. Umsetzungsreihenfolge (ein Commit pro Schritt)

1. `ActionsConfig` in `core::config`, Defaults, Tests. **Commit:** „Führe ActionsConfig ein"
2. `SharedState`: `self_filter` + `mute_store`, Tests. **Commit:** „Ergänze Selbstfilter und Mute-Speicher im SharedState"
3. `daemon/src/actions.rs`: `ActionExecutor`-Gerüst, Allow-List- und Rate-Limit-Prüfung (noch ohne echte Ausführung, liefert `Completed{dry_run:true}` für erlaubte Aktionen), Audit-Log. **Commit:** „Implementiere Allow-List, Rate-Limit und Audit-Log für Aktionen"
4. `RestartUnit`/`StopUnit` über D-Bus, Selbstfilter-Eintrag. **Commit:** „Führe Unit-Neustart/-Stopp über D-Bus aus"
5. `TerminateProcess` mit SIGTERM/SIGKILL-Eskalation. **Commit:** „Führe Prozessbeendigung mit Timeout-Eskalation aus"
6. `BlockIp` über `nft`. **Commit:** „Sperre IP-Adressen befristet über nftables"
7. `MuteAnomaly`, Pipeline-Anbindung von Selbstfilter und Mute-Store. **Commit:** „Verbinde Pipeline mit Selbstfilter und Mute-Speicher"
8. `client_task.rs` an `ActionExecutor` anschließen, `Hello` mit echten `allowed_actions`/`dry_run`. **Commit:** „Verbinde Aktionsanfragen mit dem Executor"
9. GUI: Bestätigungsdialog mit Vorschau, Audit-Ansicht, Aktions-Buttons im Detail-Panel. **Commit:** „Ergänze Aktions-UI mit Bestätigungsdialog und Audit-Ansicht"

## 9. Offene Punkte

- Echte End-to-End-Tests von `RestartUnit`/`TerminateProcess`/`BlockIp` laufen **nicht** gegen das reale System (Risiko für die laufende Maschine); Tests nutzen ausschließlich Dry-Run (Regel 26) plus, wo sinnvoll, eigens angelegte harmlose Test-Units/-Prozesse.
- `nft`-Aufrufe scheitern ohne Root/`CAP_NET_ADMIN` -- der Daemon läuft in Phase 9 privilegiert, im Entwicklungsbetrieb ohne Root liefert `BlockIp` also `Failed` mit der echten `nft`-Fehlermeldung, nicht stillschweigend Erfolg.
- Persistenz des Mute-Stores über Neustarts (analog zu Baselines) ist für v1.0 nicht vorgesehen -- ein Neustart des Daemons verwirft dauerhaft gemutete Anomalien. Nachtrag für Phase 9, falls gewünscht.
