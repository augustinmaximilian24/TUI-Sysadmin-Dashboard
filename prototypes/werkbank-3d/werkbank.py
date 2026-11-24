#!/usr/bin/env python3
"""Öffnet den Werkbank-Prototyp als eigenes Programmfenster (GTK 3 + WebKitGTK 4.1) statt im Browser."""
import json
import re
import socket
import subprocess
import sys
import threading
import time
import urllib.request
import xml.etree.ElementTree as ET
from pathlib import Path

import gi

gi.require_version("Gdk", "3.0")
gi.require_version("Gtk", "3.0")
gi.require_version("WebKit2", "4.1")
from gi.repository import Gdk, GLib, Gtk, WebKit2  # noqa: E402

# Muss zur StartupWMClass der .desktop-Datei passen, damit die Taskleiste das Fenster dem Starter zuordnet.
GLib.set_prgname("logsentry-werkbank")
Gdk.set_program_class("logsentry-werkbank")
page = Path(__file__).resolve().with_name("index.html")

# Dieselben Dateien wie die Graph-Tabs der GUI (Defaults aus core/src/config.rs). Sie werden nur zur Laufzeit
# eingespielt und landen nie im Repo; fehlt eine Datei, zeigt die Seite Beispieldaten.
GRAPHS = {
    "kg": "~/.claude/activity-log/graphify-out/graph.json",
    "home": "~/.local/share/homegraph/graph.json",
}


def load_graph(path):
    """Liest eine graph.json (NetworkX node_link_data) und verdichtet sie auf das Format der Seite."""
    try:
        g = json.loads(Path(path).expanduser().read_text(encoding="utf-8"))
    except (OSError, ValueError) as e:
        print(f"Graph nicht geladen ({path}): {e}", file=sys.stderr)
        return None
    groups, group_idx, ids, nodes = [], {}, {}, []
    for n in g.get("nodes", []):
        name = str(n.get("community_name") or n.get("community") or "ohne Gruppe")
        if name not in group_idx:
            group_idx[name] = len(groups)
            groups.append(name)
        ids[n["id"]] = len(nodes)
        nodes.append([str(n.get("label") or n["id"]), group_idx[name]])
    links = [
        [ids[l["source"]], ids[l["target"]], int(l.get("confidence") == "EXTRACTED")]
        for l in g.get("links", [])
        if l.get("source") in ids and l.get("target") in ids
    ]
    return {"groups": groups, "nodes": nodes, "links": links}


graphs = {k: v for k, v in ((k, load_graph(p)) for k, p in GRAPHS.items()) if v}
content = WebKit2.UserContentManager()
content.add_script(WebKit2.UserScript.new(
    "window.WERKBANK_GRAPHS = " + json.dumps(graphs, ensure_ascii=False) + ";",
    WebKit2.UserContentInjectedFrames.TOP_FRAME, WebKit2.UserScriptInjectionTime.START, None, None))

# Live-Werte vom Daemon (nur lesend: Hello mit Abo, danach werden nur Zeilen gelesen; keine Aktionen).
# Protokoll siehe proto/src/wire.rs: JSON-Zeilen, Version 2.
SOCKET = "/run/logsentry/collector.sock"
PROTOCOL_VERSION = 2
ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
LEVEL = {"info": "INFO", "warn": "WARN", "critical": "CRIT"}


def compact(a):
    """AnomalyEvent -> [id, Zeit ms, Stufe, Unit, Score, Template, Zeile] ohne ANSI-Farbcodes."""
    return [a["id"], a["timestamp_us"] // 1000, LEVEL.get(a["level"], "INFO"), a.get("unit") or "–",
            round(a["breakdown"]["combined"], 2), ANSI.sub("", a["template_text"])[:400],
            ANSI.sub("", a["sample_message"])[:800]]


def push(msg):
    """Reicht eine Nachricht an die Seite weiter (nur im GTK-Hauptthread)."""
    js = "window.werkbankLive && window.werkbankLive(" + json.dumps(msg, ensure_ascii=False) + ")"
    GLib.idle_add(lambda: view.evaluate_javascript(js, -1, None, None, None, None, None) and False)


def system_values(sy):
    """Systemwerte des Daemons (dieselben wie in der GUI) plus GPU-Werte, die der Daemon nicht liefert."""
    return {
        "cpu": sy["cpu"]["global_usage_percent"],
        "mem": [sy["memory"]["used_bytes"], sy["memory"]["total_bytes"], sy["memory"]["used_percent"]],
        "load": [sy["load"]["one"], sy["load"]["five"], sy["load"]["fifteen"]],
        "disks": {d["mount_point"]: d["used_percent"] for d in sy.get("disks", [])},
        "temps": {t["label"]: t["celsius"] for t in sy.get("temperatures", [])},
        "gpu": gpu_values(),
    }


def find_gpu():
    """Geräteordner der dedizierten AMD-Karte (größter VRAM), nicht der iGPU des Ryzen."""
    best = None
    for dev in Path("/sys/class/drm").glob("card[0-9]*/device"):
        try:
            if (dev / "vendor").read_text().strip() != "0x1002":
                continue
            vram = int((dev / "mem_info_vram_total").read_text())
        except (OSError, ValueError):
            continue
        if not best or vram > best[0]:
            best = (vram, dev)
    return best[1] if best else None


GPU_DEV = find_gpu()


def gpu_values():
    """Auslastung, VRAM, Leistung und Lüfter aus amdgpu-sysfs (ohne Root lesbar); fehlende Werte bleiben None."""
    if not GPU_DEV:
        return None

    def read(rel, scale=1):
        try:
            return int(next(GPU_DEV.glob(rel)).read_text()) / scale
        except (OSError, ValueError, StopIteration):
            return None

    return {"busy": read("gpu_busy_percent"), "vram": [read("mem_info_vram_used"), read("mem_info_vram_total")],
            "power": read("hwmon/hwmon*/power1_average", 1e6), "fan": read("hwmon/hwmon*/fan1_input")}


# Weltkugel: dieselben Daten wie der Netzwerk-Tab, geliefert von `logsentry-gui --netzkarte-json`
# (Verbindungen, Orte, Routen). Nur ein Build aus diesem Repo kennt den Schalter; eine ältere installierte
# GUI würde stattdessen ihr Fenster öffnen, deshalb kein Rückfall auf den PATH.
REPO = Path(__file__).resolve().parents[2]
NETMAP_BINARIES = [REPO / "target/release/logsentry-gui", REPO / "target/debug/logsentry-gui"]


def netmap_bridge():
    exe = max((b for b in NETMAP_BINARIES if b.exists()), key=lambda b: b.stat().st_mtime, default=None)
    if not exe:
        print("Weltkugel: kein logsentry-gui-Build gefunden (just werkbank baut ihn)", file=sys.stderr)
        return
    while True:
        try:
            with subprocess.Popen([str(exe), "--netzkarte-json"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                  text=True) as proc:
                for line in proc.stdout:
                    if line.startswith("{"):  # Log-Zeilen der GUI landen ebenfalls auf stdout
                        push({"t": "net", "d": json.loads(line)})
        except (OSError, ValueError) as e:
            print(f"Weltkugel-Export fehlgeschlagen: {e}", file=sys.stderr)
        time.sleep(5)


def daemon_bridge():
    hello = {"type": "hello", "protocol_version": PROTOCOL_VERSION, "client_name": "logsentry-werkbank",
             "client_version": "0.1", "subscription": {"snapshots": True, "anomalies": True, "snapshot_interval_ms": 1000}}
    while True:
        try:
            with socket.socket(socket.AF_UNIX) as s:
                s.connect(SOCKET)
                s.sendall((json.dumps(hello) + "\n").encode())
                for line in s.makefile(encoding="utf-8"):
                    m = json.loads(line)
                    t = m.get("type")
                    if t == "snapshot":
                        w = m["window"]
                        push({"t": "snap", "e": w["entropy_bits"], "z": w["entropy_z"], "n": w["events_in_window"]})
                        if m.get("system"):
                            push({"t": "sys", "s": system_values(m["system"])})
                    elif t == "recent_anomalies":
                        push({"t": "recent", "a": [compact(a) for a in m["anomalies"]]})
                    elif t == "anomaly":
                        push({"t": "anom", "a": compact(m)})
                    elif t == "goodbye":
                        print(f"Daemon beendet die Verbindung: {m.get('reason')}", file=sys.stderr)
                        if (m.get("reason") or {}).get("reason") == "version_mismatch":
                            push({"t": "offline"})
                            return  # passt nicht zu diesem Daemon; Neuversuche wären sinnlos
                        break
        except (OSError, ValueError, KeyError) as e:
            print(f"Daemon nicht erreichbar ({SOCKET}): {e}", file=sys.stderr)
        push({"t": "offline"})
        time.sleep(5)


# Router an der Wand: Ereignisprotokoll, Verbindungsstatus und Geräteliste der Fritz!Box über TR-064 (nur lesende
# Aktionen, Digest-Login mit eigenem Benutzer). Zugangsdaten liegen außerhalb des Repos in FRITZ_CRED.
FRITZ = "http://fritz.box:49000"
FRITZ_CRED = Path("~/.config/fritzbox-cred").expanduser()
FRITZ_INTERVAL = 30
# Meldungen, die auf eine Störung hindeuten. Die nächtliche Zwangstrennung („getrennt") gilt bewusst nicht als Problem.
PROBLEM = re.compile(r"antwortet nicht|Zeitüberschreitung|unzulässige Verkabelung|fehlgeschlagen|Fehler|Störung|gestört"
                     r"|nicht erreichbar|ungültig|abgelehnt|Überlast", re.IGNORECASE)
LOG_LINE = re.compile(r"(\d\d)\.(\d\d)\.(\d\d) (\d\d):(\d\d):(\d\d) (.*)")


def fritz_opener():
    cred = dict(l.split("=", 1) for l in FRITZ_CRED.read_text(encoding="utf-8").splitlines() if "=" in l)
    pm = urllib.request.HTTPPasswordMgrWithDefaultRealm()
    pm.add_password(None, FRITZ, cred["FRITZ_USERNAME"].strip(), cred["FRITZ_PASSWORD"].strip())
    return urllib.request.build_opener(urllib.request.HTTPDigestAuthHandler(pm))


def tr064(op, control, service, action):
    """Eine TR-064-Aktion ohne Argumente; liefert die New*-Ausgabewerte."""
    body = ('<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" '
            's:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body>'
            f'<u:{action} xmlns:u="{service}"/></s:Body></s:Envelope>')
    req = urllib.request.Request(FRITZ + control, body.encode(), {
        "Content-Type": 'text/xml; charset="utf-8"', "SOAPAction": f"{service}#{action}"})
    with op.open(req, timeout=10) as r:
        return {e.tag: e.text for e in ET.fromstring(r.read()).iter() if e.tag.startswith("New")}


def parse_log(text):
    """Protokollzeilen „TT.MM.JJ hh:mm:ss Text" -> [Zeit ms, Text, Problem?], neueste zuerst wie in der Box."""
    out = []
    for line in (text or "").splitlines():
        m = LOG_LINE.match(line)
        if m:
            d, mo, y, h, mi, s, msg = m.groups()
            ts = time.mktime((2000 + int(y), int(mo), int(d), int(h), int(mi), int(s), 0, 0, -1))
            out.append([int(ts * 1000), msg.strip(), bool(PROBLEM.search(msg))])
    return out


def fritz_snapshot(op):
    log = parse_log(tr064(op, "/upnp/control/deviceinfo", "urn:dslforum-org:service:DeviceInfo:1",
                          "GetDeviceLog")["NewDeviceLog"])
    st = tr064(op, "/upnp/control/wanpppconn1", "urn:dslforum-org:service:WANPPPConnection:1", "GetStatusInfo")
    path = tr064(op, "/upnp/control/hosts", "urn:dslforum-org:service:Hosts:1",
                 "X_AVM-DE_GetHostListPath")["NewX_AVM-DE_HostListPath"]
    with op.open(FRITZ + path, timeout=10) as r:
        items = ET.fromstring(r.read()).findall("Item")
    hosts = [[i.findtext("HostName") or "–", i.findtext("IPAddress") or "", i.findtext("MACAddress") or "",
              i.findtext("Active") == "1", i.findtext("InterfaceType") or "", i.findtext("X_AVM-DE_Guest") == "1",
              int(i.findtext("X_AVM-DE_Speed") or 0)] for i in items]
    return {"log": log, "conn": st.get("NewConnectionStatus"), "uptime": int(st.get("NewUptime") or 0), "hosts": hosts}


def fritz_bridge():
    while True:
        try:
            push({"t": "fritz", "d": fritz_snapshot(fritz_opener())})
        except FileNotFoundError:
            push({"t": "fritz", "d": {"err": f"Keine Zugangsdaten ({FRITZ_CRED})"}})
            return  # ohne Zugangsdaten bleibt der Router aus; kein Dauer-Neuversuch
        except (OSError, ValueError, KeyError, ET.ParseError) as e:  # urllib-Fehler sind OSError
            print(f"Fritz!Box nicht erreichbar: {e}", file=sys.stderr)
            push({"t": "fritz", "d": {"err": "Fritz!Box nicht erreichbar"}})
        time.sleep(FRITZ_INTERVAL)


def on_load(_view, event):
    if event == WebKit2.LoadEvent.FINISHED and not getattr(on_load, "started", False):
        on_load.started = True
        threading.Thread(target=daemon_bridge, daemon=True).start()
        threading.Thread(target=netmap_bridge, daemon=True).start()
        threading.Thread(target=fritz_bridge, daemon=True).start()


settings = WebKit2.Settings(
    enable_webgl=True,
    hardware_acceleration_policy=WebKit2.HardwareAccelerationPolicy.ALWAYS,
    enable_developer_extras="--debug" in sys.argv,
    enable_write_console_messages_to_stdout=True,
)
view = WebKit2.WebView(settings=settings, user_content_manager=content)
view.connect("load-changed", on_load)
view.load_uri(page.as_uri())

win = Gtk.Window(title="logsentry Werkbank")
win.set_icon_name("utilities-system-monitor")
win.set_default_size(1600, 1000)
win.add(view)
win.connect("destroy", Gtk.main_quit)
win.maximize()
win.show_all()
Gtk.main()
