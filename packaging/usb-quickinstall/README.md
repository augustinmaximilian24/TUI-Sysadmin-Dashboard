# USB-Schnellinstallation

Dieser Ordner enthält die Quellen für eine "ein Klick"-Installation von
logsentry, gedacht zum Verteilen über einen USB-Stick (oder ein
GitHub-Release) auf einen anderen Linux-PC, ohne dass man dort Terminal-
Befehle von Hand eintippen muss.

- `installieren.sh` — Kopf-Skript einer selbstentpackenden Installer-Datei.
  Escaliert sich selbst auf Root-Rechte (`pkexec`, sonst `sudo`), entpackt
  ein an sich selbst angehängtes Base64/tar.gz-Payload, ruft darin
  [`../install.sh`](../install.sh) auf und erledigt zusätzlich automatisch,
  was in der manuellen Anleitung sonst Handarbeit wäre: Benutzer zur Gruppe
  `logsentry` hinzufügen, Dienst aktivieren/starten.
- `installieren.desktop` — alternativer Startpunkt für Dateimanager, die
  rohe Skripte nicht per Doppelklick ausführen (z. B. GNOME Files/Nautilus),
  oder wenn das Zielmedium keine Unix-Ausführungsrechte speichern kann
  (FAT32-USB-Sticks). Findet `installieren.sh` relativ zu sich selbst
  (Desktop-Entry-Feld `%k`).
- `ANLEITUNG-AUTOMATISCH.md` — Anleitung für den Klick-Weg samt
  Problembehandlung (glibc-Version, FAT32-Ausführungsrechte, `noexec`-
  Mounts, fehlendes `pkexec`/`sudo`, usw.).
- `ANLEITUNG-MANUELL.md` — die bisherige Schritt-für-Schritt-Anleitung
  (siehe auch [`../install.sh`](../install.sh) direkt) als Referenz und
  Fallback.

## Fertiges Installationspaket bauen

`installieren.sh` ist hier bewusst **ohne** eingebettetes Payload
eingecheckt (ein fertig gebautes Release-Binary gehört nicht ins
Git-Repo). Um daraus die tatsächlich lauffähige, einzelne Installer-Datei
zu erzeugen:

```bash
# aus dem Projekt-Wurzelverzeichnis, nach `cargo build --release`
TAR=logsentry-<version>-linux-x86_64.tar.gz
tar -czf "$TAR" \
    -C .. \
    <passendes Verzeichnis mit target/release/{logsentry-daemon,logsentry-gui} und packaging/>

cat packaging/usb-quickinstall/installieren.sh > logsentry-installieren.sh
base64 "$TAR" >> logsentry-installieren.sh
chmod +x logsentry-installieren.sh
```

Fertig gebaute Pakete (die eigentliche `logsentry-installieren.sh` mit
eingebettetem Payload, das rohe `.tar.gz` sowie beide Anleitungen) werden
als [GitHub Release](../../../releases) bereitgestellt statt im Git-Verlauf
versioniert.
