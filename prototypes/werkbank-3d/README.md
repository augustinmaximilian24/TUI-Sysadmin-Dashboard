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
just werkbank-vorschau
```

oder direkt `xdg-open prototypes/werkbank-3d/index.html`. Läuft komplett
offline aus der Datei, ohne Server und ohne externe Anfragen.

Bedienung: Ziehen dreht, Mausrad zoomt, Klick auf PC oder Monitor zoomt
hinein, „Schreibtisch" führt zurück. Oben rechts wechselt der Knopf zwischen
System, Hell und Dunkel.

## Was beim Test auf dem Desktop zu prüfen ist

- Läuft die Szene flüssig (Grafikkarte: ASUS RX 9070 XT)? Firefox:
  `about:support` → „WebGL 2" sollte die Radeon nennen, nicht „llvmpipe".
- Stimmen Anordnung, Größenverhältnisse und Farben mit dem echten Platz überein?
- Dunkelmodus: Ist das Licht der Monitorlampe angenehm warm, sind die Lüfter
  golden genug?
- CPU-Last des Browser-Tabs im Leerlauf beobachten (`top`): der Prototyp
  zeichnet dauerhaft (drehende Lüfter). Das ist für die echte GUI **nicht**
  zulässig (Regel 20/27) und wird dort durch Neuzeichnen nur bei Änderungen
  ersetzt.

## Abhängigkeiten

`vendor/three-0.147.0/` enthält three.js 0.147.0 (MIT, Lizenz liegt daneben):
`three.min.js` sowie `OrbitControls`, `RoomEnvironment` und
`RoundedBoxGeometry` aus `examples/js`. Version 0.147 ist die letzte mit diesen
klassischen (nicht-Modul-)Skripten, die direkt per `file://` laden. Mitgeliefert
statt per CDN, damit der Prototyp offline läuft und keine Anfragen an Dritte
stellt.

Alle Bauteile sind im Code aus Grundformen nachgebaut. Fertige 3D-Modelle
(glTF) mit freier Lizenz gab es für die konkreten Teile nicht; kostenpflichtige
Modelle oder eigene Fotos als Vorlage sind die Alternative.
