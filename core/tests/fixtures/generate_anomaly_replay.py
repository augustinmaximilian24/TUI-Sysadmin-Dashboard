#!/usr/bin/env python3
"""Erzeugt die Fixture fuer den Anomalie-Replay-Test.

Deterministisch (fester Seed), damit der Test reproduzierbar bleibt.
Szenario:
  0-900 s    Normalbetrieb: wiederkehrende Log-Zeilen mehrerer Units
  900-960 s  SSH-Bruteforce: hohe Rate von "Failed password"-Zeilen
  960-1200 s Normalbetrieb
  1200 s     OOM-Kill (bis dahin nie gesehenes Template)
  1200-1500s Normalbetrieb
"""

import json
import random

random.seed(20260902)

BASE_US = 1_767_225_600_000_000  # fester Startzeitpunkt
OUT = "core/tests/fixtures/anomaly_replay.ndjson"

NORMAL = [
    ("cron.service", 6, "(root) CMD (/usr/bin/backup.sh)"),
    ("systemd-logind.service", 6, "New session {n} of user alice"),
    ("nginx.service", 6, "GET /index.html 200"),
    ("nginx.service", 6, "GET /assets/app.css 200"),
    ("chronyd.service", 6, "Selected source 192.168.1.1"),
    ("smartd.service", 6, "Device: /dev/sda [SAT], SMART Usage Attribute: 194 Temperature_Celsius changed from 32 to 33"),
]

lines = []


def emit(second, unit, prio, message, pid=None):
    entry = {
        "__REALTIME_TIMESTAMP": str(BASE_US + int(second * 1_000_000)),
        "PRIORITY": str(prio),
        "MESSAGE": message,
        "_HOSTNAME": "heimserver",
    }
    if unit:
        entry["_SYSTEMD_UNIT"] = unit
    if pid is not None:
        entry["_PID"] = str(pid)
    lines.append(json.dumps(entry))


def normal_traffic(start, end):
    """Gleichmaessige Grundlast: pro Sekunde 2-4 gewoehnliche Zeilen."""
    session = 100
    for second in range(start, end):
        for _ in range(random.randint(2, 4)):
            unit, prio, template = random.choice(NORMAL)
            message = template.replace("{n}", str(session))
            session += 1
            emit(second, unit, prio, message, pid=random.randint(500, 3000))


normal_traffic(0, 900)

# SSH-Bruteforce: 60 s lang ~20 Fehlversuche pro Sekunde von wechselnden Ports.
port = 40000
for second in range(900, 960):
    for _ in range(20):
        port += 1
        emit(
            second,
            "sshd.service",
            5,
            f"Failed password for invalid user root from 203.0.113.9 port {port} ssh2",
            pid=random.randint(4000, 4100),
        )
    for _ in range(random.randint(2, 4)):
        unit, prio, template = random.choice(NORMAL)
        emit(second, unit, prio, template.replace("{n}", "999"), pid=1234)

normal_traffic(960, 1200)

# OOM-Kill: einzelnes, bis dahin nie gesehenes Template.
emit(
    1200,
    "systemd-oomd.service",
    3,
    "Killed process 5321 (stress) due to memory pressure for /user.slice",
    pid=1,
)

normal_traffic(1201, 1500)

with open(OUT, "w") as handle:
    handle.write("\n".join(lines) + "\n")

print(f"{len(lines)} Zeilen nach {OUT} geschrieben")
