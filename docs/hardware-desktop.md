# Hardware des Desktop-Rechners (Linux Mint, Zielsystem „primär")

Quelle: Bestellübersicht des Komplett-PCs (Systemtreff, Artikelnummer
30232243s) plus spätere Umbauten. Grundlage für die 3D-Werkbank
(Modelle, Beschriftungen) und für die Zuordnung der Sensoren zu Bauteilen.

| Bauteil | Modell | Sensor / Datenquelle unter Linux |
|---|---|---|
| Gehäuse | Systemtreff Gaming Case Plexus-ST-801, schwarz (Glas-Seitenteil, 3 Front-Lüfter mit Leuchtring, 1 Heck-Lüfter) | – |
| CPU | AMD Ryzen 5 7600X, 6 Kerne, 4,7 GHz (Boost bis 5,3 GHz), AM5 | `k10temp` (Tctl, Tccd1) |
| CPU-Kühler | Arctic Freezer 34 eSports DUO (Turmkühler, 2 × 120-mm-Lüfter Push/Pull) | Lüfterdrehzahl über Mainboard-hwmon (`fan*_input`), noch nicht erfasst |
| Mainboard | MSI B650 GAMING PLUS WIFI (ATX, DDR5, AM5) | LAN `r8169`, WLAN `mt7921` |
| Arbeitsspeicher | 32 GB DDR5-6000 CL30 Kingston FURY Beast (2 × 16 GB) | `spd5118` |
| Grafikkarte | **AMD Radeon RX 9070 XT OC** (ersetzt die ursprünglich verbaute Nvidia GeForce RTX 4060 Ti 16 GB) | `amdgpu`-hwmon (edge/junction/mem, Lüfter, Leistung), noch nicht erfasst |
| Netzteil | Gigabyte UD750GM, 750 W, 80+ Gold, modular | keine Sensoren |
| SSD | Lexar NM790, 2 TB, M.2 NVMe PCIe 4.0 | `nvme` (Composite, Sensor 1/2) |
| Peripherie | Razer-Geräte (über `openrazer`), Tastenfeld über OpenDeck, 3 Monitore (einer mit 180 Hz) | – |

Hinweise:
- Laut Bestellung wurde die CPU gegen den 7600X getauscht (Titel nennt noch 7500F).
- Der Autostart-Eintrag „NVIDIA Performance Mode" stammt noch von der alten
  Nvidia-Karte und ist mit der RX 9070 XT überflüssig.
- Offen: genaue Marke der RX 9070 XT OC, Lüfterbelegung am Mainboard,
  Monitor-Modelle und ihre Anordnung.
