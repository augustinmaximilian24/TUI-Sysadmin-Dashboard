# Hardware des Desktop-Rechners (Linux Mint, Zielsystem „primär")

Quelle: Bestellübersicht des Komplett-PCs (Systemtreff, Artikelnummer
30232243s) plus spätere Umbauten. Grundlage für die 3D-Werkbank
(Modelle, Beschriftungen) und für die Zuordnung der Sensoren zu Bauteilen.

| Bauteil | Modell | Sensor / Datenquelle unter Linux |
|---|---|---|
| Gehäuse | Zweikammer-Gehäuse, schwarz, Front und linke Seite aus Glas (ohne Eckstrebe); 3 × 120 mm am Boden (Intake), 3 × 120 mm im Deckel (Outtake), Leuchtringe aktuell **golden**; Netzteil in der hinteren Kammer, von vorne nicht sichtbar. Bestellt war das Systemtreff Plexus-ST-801 | – |
| CPU | AMD Ryzen 5 7600X, 6 Kerne, 4,7 GHz (Boost bis 5,3 GHz), AM5 | `k10temp` (Tctl, Tccd1) |
| CPU-Kühler | Arctic Freezer 34 eSports DUO (Turmkühler, 2 × 120-mm-Lüfter Push/Pull); Kühlkörper schwarz, Lüfter weiß | Lüfterdrehzahl über Mainboard-hwmon (`fan*_input`), noch nicht erfasst |
| Mainboard | MSI B650 GAMING PLUS WIFI (ATX, DDR5, AM5) | LAN `r8169`, WLAN `mt7921` |
| Arbeitsspeicher | 32 GB DDR5-6000 CL30 Kingston FURY Beast (2 × 16 GB) | `spd5118` |
| Grafikkarte | **ASUS Radeon RX 9070 XT OC** (ersetzt die ursprünglich verbaute Nvidia GeForce RTX 4060 Ti 16 GB) | `amdgpu`-hwmon (edge/junction/mem, Lüfter, Leistung), noch nicht erfasst |
| Netzteil | Gigabyte UD750GM, 750 W, 80+ Gold, modular | keine Sensoren |
| SSD | Lexar NM790, 2 TB, M.2 NVMe PCIe 4.0 | `nvme` (Composite, Sensor 1/2) |
| Monitore | 34 Zoll Curved-Ultrawide als Hauptmonitor unten mittig; 27 Zoll mittig darüber; 27 Zoll hochkant rechts neben beiden (einer davon mit 180 Hz) | – |
| Peripherie | Razer-Tastatur und -Maus (über `openrazer`), Tastenfeld über OpenDeck, Standmikrofon links; der PC steht rechts auf dem Schreibtisch, Glasseite zur Tischmitte | – |

Hinweise:
- Laut Bestellung wurde die CPU gegen den 7600X getauscht (Titel nennt noch 7500F).
- Der Autostart-Eintrag „NVIDIA Performance Mode" stammt noch von der alten
  Nvidia-Karte und ist mit der RX 9070 XT überflüssig.
- Offen: genaue Serie der ASUS-Karte (TUF Gaming oder Prime), Lüfterbelegung
  am Mainboard, Monitor-Modelle.
