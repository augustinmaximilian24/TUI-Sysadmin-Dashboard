# Phase 14 – graphsync: Entwurf und Abwägungen

## 1. Ziel

Die Verknüpfungen der Graph-Ansichten sollen ohne manuellen Auftrag an eine
KI aktuell bleiben. Auslöser ist eine Dateiänderung, nicht ein Mensch.
Verarbeitet wird nur das Delta, nie der ganze Bestand.

## 2. Wo es läuft

| Option | Entscheidung |
|---|---|
| im Daemon | **nein**: Der Daemon ist privilegiert und läuft mit `ProtectSystem=strict`. Die Notizen liegen im Home. Abschnitt 4 trennt Daemon und Benutzerdaten verpflichtend. |
| in der GUI | **nein**: Die Aktualisierung soll auch ohne offene GUI laufen. |
| eigener Benutzerprozess | **ja**: `logsentry-graphsync` als systemd-User-Unit, ohne root, mit der eigenen Claude-Code-Anmeldung. |

Die GUI bleibt reiner Leser. Sie mischt eine zweite Datei (Overlay) in den
vorhandenen Graphen und schreibt nie selbst.

## 3. Warum ein Overlay statt `graph.json` zu ändern

`graph.json` gehört anderen Werkzeugen (graphify bzw. `homegraph.py`) und
kann jederzeit überschrieben werden. graphsync schreibt deshalb nach
`~/.local/share/logsentry/graphsync/links.json`, im selben NetworkX-Format.
Beim Laden ordnet die GUI die Knoten zu (`gui/src/knowledge_graph/data.rs`,
`merge_overlay`):

- Die Zuordnung läuft über den Dateipfad: gleicher `source_file`, oder ein
  relativer Basispfad, auf den der Overlay-Pfad endet. Absolute Pfade als
  Knoten-ID zählen ebenfalls.
- Bei mehreren Konzepten pro Datei gewinnt das Label, das dem Dateinamen
  entspricht.
- Nicht zugeordnete Notizen werden im Wissensgraphen ergänzt, in der
  Home-Übersicht nicht (dort nur verbinden). Das stellen
  `overlay_adds_nodes` und `overlay_max_added_nodes` ein. Das 3D-Layout ist
  O(n²): 2000 Knoten brauchen im Release-Build etwa 2,5 s.
- Pro Knotenpaar bleibt die Kante des Basisgraphen erhalten.

## 4. Delta-Erkennung

- Vorfilter über Größe und mtime (in ns). Ungeänderte Dateien kosten nur
  ein `stat()`.
- Inhaltshash mit FNV-1a aus `core::hash`. Er ist nicht kryptografisch,
  muss es für Änderungserkennung aber nicht sein. Dafür braucht es keine
  neue Abhängigkeit (`blake3` wurde erwogen und verworfen).
- Die Zustandsdatei ist JSON statt `redb`, denn sie wird immer komplett
  gelesen und geschrieben, ist durch `max_files` begrenzt und lässt sich
  von Hand inspizieren. Geschrieben wird atomar (tmp + `rename`). Sie hat
  eine `schema_version`. Ist sie unlesbar oder neuer als erwartet, wird sie
  als `.bak-<zeit>` gesichert und neu begonnen. Die Offline-Stufen bauen
  dann alles beim nächsten Lauf wieder auf.
- Die Ausgabe wird nur geschrieben, wenn sie sich byte-genau unterscheidet.
  Sonst würde die GUI unnötig neu layouten.

## 5. Kantenquellen und Priorität

| Quelle | `confidence` | `relation` |
|---|---|---|
| `[[Wikilink]]`, `[Text](pfad.md)` | `EXTRACTED` | `links_to` |
| KI (`claude -p`) | `INFERRED` | vom Modell, bereinigt auf `[a-z0-9_]` |
| gemeinsame seltene Tags/Überschriftenwörter | `INFERRED` | `shares_tag` / `shares_topic` |

Bei Erreichen von `max_edges` fallen zuerst die Begriffs-Kanten weg, dann
die KI-Kanten. Für die Begriffs-Kanten gibt ein Tag 2 Punkte und ein
Überschriftenwort 1 Punkt. Begriffe in mehr als `max_term_docs` Dateien
zählen nicht. Das ist reines Zählen und kein TF-IDF (ML-Modelle sind laut
Abschnitt 2 out of scope). Zugleich begrenzt es die Paaranzahl je Begriff
auf `max_term_docs²/2`.

## 6. KI-Stufe: Sicherheitsmodell

Notizinhalte sind nicht vertrauenswürdig, denn sie könnten Anweisungen
enthalten (Prompt-Injection). Darum:

1. Der Prozess wird direkt gestartet (`tokio::process::Command`, keine
   Shell). Der Prompt geht über stdin, der Inhalt nie in Argumente.
2. `--tools ""` schaltet alle Werkzeuge ab, `--strict-mcp-config` lädt
   keine MCP-Server, und das Arbeitsverzeichnis ist leer. Das Modell kann
   nur Text zurückgeben.
3. Dateien heißen im Prompt `F1`, `F2` usw. Angenommen werden nur Kanten,
   deren Quelle eine analysierte Datei und deren Ziel eine bekannte ID ist.
   Ein frei erfundener Pfad wie `/etc/passwd` wird verworfen. Marker
   (`<<<`) im Inhalt werden entschärft.
4. Zeitlimit mit `kill_on_drop` und eine begrenzte Ausgabe. stderr wird
   vollständig geleert, damit der Prozess nie an einer vollen Pipe hängt.

Verifiziert wurde das Ausgabeformat von `claude -p --output-format json`
mit der installierten CLI (2.1.x):
`{"type":"result","is_error":false,"result":"…","total_cost_usd":…}`.

## 7. KI-Stufe: Budget

| Grenze | Default | Bedeutung |
|---|---|---|
| `max_runs_per_day` | 6 | Aufrufe pro lokalem Kalendertag, fehlgeschlagene zählen mit |
| `max_usd_per_day` | 0,50 | laut `total_cost_usd`; die Prüfung erfolgt vor dem Aufruf, ein Lauf kann die Grenze also einmal überschreiten |
| `max_files_per_run` | 10 | Batchgröße |
| `min_stable_secs` | 600 | eine Datei wird erst analysiert, wenn sie 10 min nicht mehr bearbeitet wurde |
| `min_interval_secs` | 1800 | Abstand zwischen erfolgreichen Aufrufen |
| `failure_backoff_secs` | 1800 | Wartezeit nach einem Fehler (z. B. kein Netz) |

Rechnung mit den Defaults: höchstens 60 Dateien pro Tag und höchstens
0,50 USD (zuzüglich eines letzten Laufs). Ein Ende-zu-Ende-Lauf mit 6
Fixture-Notizen kostete etwa 0,02 USD. Ein großer Altbestand wird also
über mehrere Tage abgearbeitet, die neuesten Dateien zuerst. Die
Offline-Kanten sind sofort da.

Liefert das konfigurierte Programm keine Kosten (anderes Ausgabeformat),
greift nur die Grenze für die Aufrufzahl.

## 8. Bekannte Grenzen

- Die Zuordnung zu graphify-Knoten ist eine Heuristik über Dateipfade.
  Ohne `source_file` im Basisgraphen werden Notizen als eigene Knoten
  ergänzt (Wissensgraph) oder ignoriert (Home-Übersicht).
- Während eines laufenden KI-Aufrufs wird SIGTERM erst nach dessen Ende
  bzw. Zeitlimit bemerkt. systemd beendet den Prozess spätestens nach
  `TimeoutStopSec`, der Kindprozess stirbt dabei mit.
- Alte KI-Kanten einer geänderten Datei bleiben bis zur nächsten Analyse
  sichtbar. Das ist bewusst so, damit der Graph während des Wartens auf
  Stabilität bzw. Budget nicht ausdünnt.
