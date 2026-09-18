# logsentry installieren (von USB-Stick) — Schnellinstallation

Diese Anleitung installiert logsentry mit einem einzigen Klick, ohne dass
man Terminal-Befehle von Hand eintippen muss. Für den manuellen Weg (z. B.
als Referenz oder auf einem PC ohne grafische Oberfläche) siehe
`logsentry-ANLEITUNG-MANUELL.md`.

## Voraussetzungen auf dem Ziel-PC

- Linux, x86_64 (64-Bit), systemd
- glibc **2.39 oder neuer** (z. B. Linux Mint 22, Ubuntu 24.04, Debian 13
  oder neuer)
- Eine grafische Desktop-Umgebung
- Administratorrechte (das eigene Benutzerkonto muss `sudo` benutzen dürfen,
  bzw. beim Passwort-Dialog das Passwort des Kontos kennen)

## Wichtiger Hinweis zum Stick

Der USB-Stick ist FAT32-formatiert. FAT32 kann keine Unix-Ausführungsrechte
speichern — die Dateien landen also immer ohne "ausführbar"-Markierung,
egal was man tut. Deshalb unten **zuerst `logsentry-installieren.desktop`
probieren** (Schritt 2a) — die kommt damit von Haus aus klar. Das rohe
Skript (Schritt 2b) braucht dagegen einen kurzen manuellen Zwischenschritt.

## So geht's

1. USB-Stick am Ziel-PC anschließen und öffnen.
2. Eine der beiden Dateien starten:

   **2a. `logsentry-installieren.desktop` (empfohlen, meist ohne
   Zusatzschritt)**
   Doppelklicken. Falls eine Sicherheitswarnung kommt ("Starten erlauben?"
   / "Vertrauen"): bestätigen, dann nochmal doppelklicken. Funktioniert bei
   den meisten Dateimanagern trotz FAT32 direkt.

   **2b. `logsentry-installieren.sh` (Alternative, falls 2a nicht
   funktioniert)**
   Erst ausführbar machen — Rechtsklick auf die Datei → Eigenschaften →
   Rechte/Berechtigungen → "Ausführen"/"Als Programm ausführen erlauben"
   aktivieren. Falls dieser Haken dort ausgegraut ist (kommt auf FAT32
   vor): die Datei zuerst auf die Festplatte kopieren, dort den Haken
   setzen bzw. im Terminal `chmod +x logsentry-installieren.sh`, und von
   dort starten. Danach doppelklicken; falls der Dateimanager fragt,
   "Ausführen" bzw. "Im Terminal ausführen" wählen (nicht "Anzeigen").

3. Es öffnet sich ein Terminalfenster mit einer Passwort-Abfrage. Passwort
   des eigenen Benutzerkontos eingeben.
4. Das Skript installiert alles automatisch: Daemon, GUI, systemd-Service,
   Gruppenmitgliedschaft, Dienst wird gestartet. Am Ende erscheint eine
   Erfolgsmeldung.
5. **Einmal ab- und wieder anmelden** (wichtig — siehe unten, warum).
6. GUI starten: über das Anwendungsmenü ("logsentry") oder im Terminal mit
   `logsentry-gui`.

## Warum einmal ab-/anmelden nötig ist

Die GUI darf sich nur mit dem Daemon verbinden, wenn der eigene Benutzer in
der Gruppe `logsentry` ist. Das Skript trägt den Benutzer zwar automatisch
in diese Gruppe ein, aber Linux übernimmt neue Gruppenmitgliedschaften erst
bei der nächsten Anmeldung — nicht sofort in der laufenden Sitzung.

## Problembehandlung

**`logsentry-installieren.sh` öffnet sich im Texteditor statt auszuführen,
oder die Option "Ausführen" fehlt/ist ausgegraut**
Erwartbar auf diesem FAT32-Stick, siehe "Wichtiger Hinweis zum Stick" oben
— einfach stattdessen `logsentry-installieren.desktop` verwenden (Schritt
2a), die braucht kein Ausführungsbit.

**Bei der `.desktop`-Datei passiert nichts, oder es kommt eine
Sicherheitswarnung**
Manche Dateimanager verlangen beim allerersten Start einer `.desktop`-Datei
von einem USB-Stick eine einmalige Bestätigung ("Starten erlauben" /
"Vertrauen"). Rechtsklick auf die Datei → entsprechende Option wählen,
danach nochmal doppelklicken.

**Kein Passwort-Dialog erscheint, oder das Skript bricht mit "kein
pkexec/sudo gefunden" ab**
Auf diesem System fehlt sowohl `pkexec` als auch `sudo` (selten, z. B. auf
sehr minimalen Installationen). Dann bleibt nur die manuelle Installation
gemäß `logsentry-ANLEITUNG-MANUELL.md` (als root anmelden, z. B. mit `su`).

**Fehlermeldung über eine fehlende `GLIBC_...`-Version, oder das Programm
lässt sich gar nicht starten**
Die glibc-Version des Ziel-PCs ist zu alt (siehe Mindestanforderung oben).
Die mitgelieferten Binaries laufen dann nicht — Programm müsste aus dem
Quellcode neu gebaut werden (Repo
`augustinmaximilian24/TUI-Sysadmin-Dashboard`).

**Skript lässt sich trotz allem nicht ausführen (auch `.desktop` nicht)**
Manche Systeme hängen USB-Sticks zusätzlich mit der Option `noexec` ein
(Programme dürfen dann grundsätzlich nicht direkt vom Stick ausgeführt
werden, unabhängig von Rechten). Abhilfe: `logsentry-installieren.sh` auf
die Festplatte kopieren (z. B. in den Download- oder Home-Ordner), dort im
Terminal `chmod +x logsentry-installieren.sh` ausführen und von dort aus
starten.

**GUI startet, zeigt aber keine Verbindung zum Daemon / bleibt leer**
Meistens fehlt noch das Ab-/Anmelden nach der Installation (siehe oben).
Falls es danach immer noch nicht geht: prüfen, ob der Dienst läuft:

```bash
systemctl status logsentry.service
```

**Am Ende steht eine Fehlermeldung zum systemd-Dienst**
Details ansehen mit:

```bash
systemctl status logsentry.service
journalctl -u logsentry.service --no-pager -n 50
```

**Meldung, dass eine Konfigurationsdatei bereits existiert und nicht
überschrieben wurde**
Das ist kein Fehler: Falls unter `/etc/logsentry/logsentry.toml` schon eine
Konfiguration liegt (z. B. von einer früheren Installation), lässt das
Skript sie bewusst unangetastet, damit keine eigenen Einstellungen
verloren gehen.

**Sonstige/unklare Fehler**
Auf die manuelle Installation ausweichen (`logsentry-ANLEITUNG-MANUELL.md`)
— dort läuft jeder Schritt einzeln und sichtbar im Terminal ab, das macht
die Fehlersuche einfacher.

## Wichtig: dry_run

In `/etc/logsentry/logsentry.toml` steht standardmäßig `dry_run = true` —
der Daemon protokolliert mögliche Aktionen (z. B. Prozess beenden, Unit
neu starten) nur, führt sie aber **nicht aus**. Das sollte man nur bewusst
auf `false` stellen, nachdem man die Konfiguration (`[actions]`-Abschnitt,
`allowed_kinds`, `allowed_units`) geprüft hat.

## Enthalten in diesem Paket

- `logsentry-installieren.sh` — die eigentliche Schnellinstallation
  (enthält Binaries + Konfiguration eingebettet)
- `logsentry-installieren.desktop` — alternativer Startpunkt für
  Dateimanager, die rohe Skripte nicht per Doppelklick ausführen

Quelltext und weitere Doku: siehe GitHub-Repo `augustinmaximilian24/TUI-Sysadmin-Dashboard`.
