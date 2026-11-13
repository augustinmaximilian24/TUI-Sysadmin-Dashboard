# Werkbank 3D – Prototyp

Klickbarer Entwurf der neuen Hauptseite: der eigene Schreibtisch in 3D
(34"-Curved unten mittig, 27" darüber, 27" hochkant rechts, PC rechts auf dem
Tisch). Klick auf einen Monitor zoomt auf den Bildschirm, Klick auf den PC
zoomt ins Gehäuse, dort ist jedes Bauteil anklickbar und zeigt seine Werte.
Hell- und Dunkelmodus; im Dunkeln beleuchtet die Monitorlampe den Tisch.

Reiner Browser-Prototyp, **kein Teil der Rust-Binaries** und ohne Verbindung
zum Daemon: die Messwerte sind eine feste Momentaufnahme. Er dient dazu,
Aussehen und Bedienung auf dem echten Rechner zu testen, bevor die Werkbank in
die egui-GUI wandert (siehe Phase 17 in `CLAUDE.md`).

## Starten

```sh
just werkbank                # eigenes Programmfenster (GTK + WebKitGTK), mit echten Graphen
just werkbank-installieren   # Eintrag „logsentry Werkbank" im Startmenü, ohne sudo
just werkbank-vorschau       # im Browser, Graphen dann mit Beispieldaten
```

Läuft komplett offline aus der Datei, ohne Server und ohne externe Anfragen.
`werkbank.py --debug` schaltet die WebKit-Entwicklerwerkzeuge frei.

Bedienung: Ziehen dreht, Mausrad zoomt, Klick auf PC oder Monitor zoomt
hinein, „Schreibtisch" führt zurück. Oben rechts wechselt der Knopf zwischen
System, Hell und Dunkel.

## Was beim Test auf dem Desktop zu prüfen ist

- Läuft die Szene flüssig (Grafikkarte: ASUS RX 9070 XT)? Firefox:
  `about:support` → „WebGL 2" sollte die Radeon nennen, nicht „llvmpipe".
- Stimmen Anordnung, Größenverhältnisse und Farben mit dem echten Platz überein?
- Dunkelmodus: Ist das Licht der Monitorlampe angenehm warm, sind die Lüfter
  golden genug?
- CPU-Last des Browser-Tabs im Leerlauf beobachten (`top`). Mit „Lüfter: an"
  zeichnet der Prototyp dauerhaft (gemessen ≈ 76 % eines Kerns in Firefox,
  ≈ 6 % der Gesamt-CPU). Mit „Lüfter: aus" (Knopf oben rechts, bleibt
  gespeichert) wird nur bei Bedienung neu gezeichnet: ≈ 0 % im Leerlauf.
  So muss es auch in der echten GUI sein (Regel 20/27).

## Abhängigkeiten

`vendor/three-0.147.0/` enthält three.js 0.147.0 (MIT, Lizenz liegt daneben):
`three.min.js` sowie `OrbitControls`, `RoomEnvironment` und
`RoundedBoxGeometry` aus `examples/js`. Version 0.147 ist die letzte mit diesen
klassischen (nicht-Modul-)Skripten, die direkt per `file://` laden. Mitgeliefert
statt per CDN, damit der Prototyp offline läuft und keine Anfragen an Dritte
stellt.

`world-110m.js` enthält die Außenringe der Länder aus
`gui/assets/world_countries_110m.geo.json` (dieselbe Datei wie der
Netzwerk-Tab), auf 0,1° gerundet. Als Skript statt JSON, weil `file://`
kein `fetch` erlaubt.

Die drei Bildschirme folgen dem Stil der GUI (`gui/src/theme.rs`) und sind
immer dunkel wie die App. Ein Klick auf einen Bildschirminhalt öffnet eine
Detailseite in der Werkbank (statt eigener Tabs), Esc führt zurück:

- **34"**: Wissensgraph und Home-Übersicht. Wie in der GUI zuerst nur die
  Gruppen als große Punkte mit zusammengefassten Kanten; Klick auf eine Gruppe
  fächert ihre Knoten auf, Klick auf einen Knoten zeigt seine Nachbarn. Die
  `graph.json` von graphify und homegraph liest `werkbank.py` beim Start und
  spielt sie nur zur Laufzeit ein; sie liegen nie im Repo.
- **27" oben**: Weltkugel mit allen Endstandorten und der Zahl der
  Verbindungen. Die Daten liefert die GUI selbst: `logsentry-gui
  --netzkarte-json` startet ohne Fenster dieselben Hintergrund-Threads wie der
  Netzwerk-Tab (Verbindungen, GeoIP/Stadt-Datenbank, Reverse-DNS, Traceroute)
  und schreibt alle 2 s eine JSON-Zeile; `werkbank.py` startet diesen Modus aus
  `target/release` bzw. `target/debug` (nie die installierte GUI, die den
  Schalter noch nicht kennt). Die Detailseite ist ein Nachbau von `draw_map`
  (`gui/src/network_map/`): gleiche Kamera, Farben, Routen über die gemessenen
  Zwischenstationen, Länderkürzel ab Zoom 1,6 und dieselbe Verbindungsliste.
- **27" hochkant**: Entropie live (letzte 2 min) und die Anzahl CRIT, WARN und
  INFO der letzten 60 min; die Seite zeigt den Verlauf der letzten 5 min und
  alle Anomalien mit Filter und Details.

Entropie und Anomalien kommen im Programm live vom Daemon
(`/run/logsentry/collector.sock`, Protokoll 2, nur lesend: Hello mit Abo,
keine Aktionen; Mitgliedschaft in der Gruppe `logsentry` nötig). Ohne Daemon
oder im Browser zeigt die Seite Beispielwerte, die Kopfzeile sagt, welches
von beiden. Kopfzeile und Bauteile zeigen dieselben Systemwerte wie die GUI
(Snapshot des Daemons); GPU-Auslastung, VRAM, Leistung und Lüfter liest
`werkbank.py` aus amdgpu-sysfs, weil der Daemon sie nicht liefert.
Gemessen mit „Lüfter: aus", Live-Daten und Netzkarten-Prozess: ≈ 2,3 % eines
Kerns zusammen.

Alle Bauteile sind im Code aus Grundformen nachgebaut. Fertige 3D-Modelle
(glTF) mit freier Lizenz gab es für die konkreten Teile nicht; kostenpflichtige
Modelle oder eigene Fotos als Vorlage sind die Alternative.
