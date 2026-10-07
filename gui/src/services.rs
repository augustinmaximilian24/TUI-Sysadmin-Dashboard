//! Karte "Eigene Dienste & Autostart" (unter den Units im Systemzustands-Panel).
//!
//! Liest unter dem Benutzerkonto (Regel 7, kein root): User-Services und
//! -Timer per `systemctl --user` sowie die Autostart-Einträge in
//! `~/.config/autostart`. Alles Parsen ist reine Funktion über Text, damit
//! es ohne laufendes systemd testbar ist; nur [`collect`] fasst das System an
//! und läuft im Hintergrund-Thread von [`ServicesMonitor`] (Regel 21).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;
use logsentry_core::config::ServicesConfig;

/// Woher ein Eintrag stammt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Service,
    Timer,
    Autostart,
}

/// Laufstatus und damit Farbe: Running grün, Failed rot, NotRunning
/// bernstein (sollte laufen, tut es nicht), Idle neutral grau (einmalig
/// bzw. nicht dauerhaft gedacht, zu Recht gerade nicht aktiv).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    Failed,
    NotRunning,
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub kind: Kind,
    pub status: Status,
    /// Kurztext, z. B. `active/running`.
    pub detail: String,
}

/// Wertet die Ausgabe von
/// `systemctl --user show <units> -p Id -p Type -p ActiveState -p SubState`
/// aus (ein Block je Unit, getrennt durch Leerzeilen).
pub fn parse_show(output: &str) -> Vec<Entry> {
    output
        .split("\n\n")
        .filter_map(|block| {
            let field = |key: &str| {
                block
                    .lines()
                    .find_map(|l| l.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
            };
            let name = field("Id").filter(|n| !n.is_empty())?;
            // Nicht installierte oder maskierte Units sind kein Befund.
            if matches!(field("LoadState"), Some("not-found" | "masked" | "error")) {
                return None;
            }
            let (active, sub) = (field("ActiveState")?, field("SubState")?);
            let enabled = field("UnitFileState").is_some_and(|s| s.starts_with("enabled"));
            let kind = if name.ends_with(".timer") {
                Kind::Timer
            } else {
                Kind::Service
            };
            let oneshot = field("Type") == Some("oneshot");
            let status = match (active, sub) {
                ("failed", _) => Status::Failed,
                ("active", "exited") => Status::Idle,
                ("active" | "activating" | "reloading", _) => Status::Running,
                _ if oneshot || !enabled => Status::Idle,
                _ => Status::NotRunning,
            };
            Some(Entry {
                name: name.to_string(),
                kind,
                status,
                detail: format!("{active}/{sub}"),
            })
        })
        .collect()
}

/// Ein Autostart-Eintrag aus `~/.config/autostart/*.desktop`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Autostart {
    pub name: String,
    pub exec: String,
}

/// Liest eine `.desktop`-Datei. `None` für abgeschaltete Einträge
/// (`Hidden=true`, `X-GNOME-Autostart-enabled=false`) oder ohne Name/Exec.
pub fn parse_desktop(text: &str) -> Option<Autostart> {
    let value = |key: &str| text.lines().find_map(|l| l.strip_prefix(key));
    if value("Hidden=") == Some("true") || value("X-GNOME-Autostart-enabled=") == Some("false") {
        return None;
    }
    let exec = value("Exec=")?
        .split_whitespace()
        .filter(|t| !t.starts_with('%'))
        .collect::<Vec<_>>()
        .join(" ");
    Some(Autostart {
        name: value("Name=")?.to_string(),
        exec,
    })
    .filter(|a| !a.exec.is_empty())
}

/// Läuft der Autostart-Eintrag? `procs` sind die Kommandozeilen aller
/// Prozesse (Argumente durch Leerzeichen verbunden).
pub fn is_running(exec: &str, procs: &[String]) -> bool {
    let basename = |t: &str| t.rsplit('/').next().unwrap_or(t).to_string();
    let Some(first) = exec.split_whitespace().next() else {
        return false;
    };
    let mut prog = basename(first);
    if prog == "flatpak" {
        // Flatpak startet das Programm unter dem Namen aus `--command=`.
        if let Some(cmd) = exec
            .split_whitespace()
            .find_map(|t| t.strip_prefix("--command="))
        {
            prog = cmd.to_string();
        }
    }
    if matches!(prog.as_str(), "sh" | "bash" | "dash") {
        // Shell-Wrapper: jede Shell hieße "sh", also nur exakte Kommandozeile.
        let key = exec.replace(['\'', '"'], "");
        return procs.contains(&key);
    }
    // Programm als argv[0] oder Script hinter einem Interpreter (argv[1]).
    procs
        .iter()
        .any(|p| p.split_whitespace().take(2).any(|t| basename(t) == prog))
}

/// Autostart-Eintrag als [`Entry`]: läuft er, ist er grün; läuft er nicht,
/// ist das neutral (Idle), denn viele Autostarts sind einmalige Scripts.
pub fn autostart_entry(a: &Autostart, procs: &[String]) -> Entry {
    let running = is_running(&a.exec, procs);
    Entry {
        name: a.name.clone(),
        kind: Kind::Autostart,
        status: if running {
            Status::Running
        } else {
            Status::Idle
        },
        detail: if running { "läuft" } else { "nicht aktiv" }.to_string(),
    }
}

/// Blendet Desktop-Infrastruktur (Namenspräfixe) aus, außer `show_all`.
pub fn visible<'a>(
    entries: &'a [Entry],
    hide_prefixes: &[String],
    show_all: bool,
) -> Vec<&'a Entry> {
    entries
        .iter()
        .filter(|e| show_all || !hide_prefixes.iter().any(|p| e.name.starts_with(p.as_str())))
        .collect()
}

/// Ergebnis einer Abfrage; `error` statt stillem Verschlucken (Regel 16).
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub entries: Vec<Entry>,
    pub error: Option<String>,
}

fn systemctl_user(args: &[&str]) -> Result<String, String> {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map_err(|e| format!("systemctl: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn process_cmdlines() -> Vec<String> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .bytes()
                .all(|b| b.is_ascii_digit())
        })
        .filter_map(|e| std::fs::read(e.path().join("cmdline")).ok())
        .map(|raw| {
            String::from_utf8_lossy(&raw)
                .replace('\0', " ")
                .trim()
                .to_string()
        })
        .filter(|c| !c.is_empty())
        .collect()
}

/// Fragt systemd (Benutzer-Instanz), das Autostart-Verzeichnis und die
/// Prozessliste ab. Blockiert kurz, läuft nur im Hintergrund-Thread.
pub fn collect(autostart_dir: &Path) -> Snapshot {
    let mut snap = Snapshot::default();
    match systemctl_user(&[
        "list-units",
        "--type=service,timer",
        "--all",
        "--no-legend",
        "--plain",
    ]) {
        Ok(listing) => {
            let units: Vec<&str> = listing
                .lines()
                .filter_map(|l| {
                    l.split_whitespace()
                        .find(|t| t.ends_with(".service") || t.ends_with(".timer"))
                })
                .collect();
            let mut args = vec![
                "show",
                "-p",
                "Id",
                "-p",
                "Type",
                "-p",
                "LoadState",
                "-p",
                "UnitFileState",
                "-p",
                "ActiveState",
                "-p",
                "SubState",
            ];
            args.extend(&units);
            match systemctl_user(&args) {
                Ok(shown) => snap.entries = parse_show(&shown),
                Err(e) => snap.error = Some(e),
            }
        }
        Err(e) => snap.error = Some(e),
    }
    let procs = process_cmdlines();
    if let Ok(dir) = std::fs::read_dir(autostart_dir) {
        for f in dir
            .flatten()
            .filter(|f| f.path().extension().is_some_and(|x| x == "desktop"))
        {
            if let Some(a) = std::fs::read_to_string(f.path())
                .ok()
                .as_deref()
                .and_then(parse_desktop)
            {
                snap.entries.push(autostart_entry(&a, &procs));
            }
        }
    }
    // Problemfälle zuerst, dann nach Name.
    let rank = |s: Status| match s {
        Status::Failed => 0,
        Status::NotRunning => 1,
        Status::Running => 2,
        Status::Idle => 3,
    };
    snap.entries.sort_by(|a, b| {
        rank(a.status)
            .cmp(&rank(b.status))
            .then_with(|| a.name.cmp(&b.name))
    });
    snap
}

/// Hält den letzten [`Snapshot`], aktualisiert von einem Hintergrund-Thread.
pub struct ServicesMonitor {
    shared: Arc<Mutex<Snapshot>>,
}

impl ServicesMonitor {
    pub fn start(config: &ServicesConfig, ctx: &egui::Context) -> Self {
        let shared = Arc::new(Mutex::new(Snapshot::default()));
        let (thread_shared, ctx) = (Arc::clone(&shared), ctx.clone());
        let interval = Duration::from_secs(config.poll_interval_seconds.max(1));
        let dir = std::env::var_os("HOME")
            .map_or_else(PathBuf::new, PathBuf::from)
            .join(".config/autostart");
        std::thread::spawn(move || loop {
            let snap = collect(&dir);
            if let Ok(mut guard) = thread_shared.lock() {
                *guard = snap;
            }
            ctx.request_repaint();
            std::thread::sleep(interval);
        });
        Self { shared }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.shared.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHOW: &str = "\
Id=logsentry-graphsync.service
LoadState=loaded
UnitFileState=enabled
Type=simple
ActiveState=active
SubState=running

Id=homegraph.service
LoadState=loaded
UnitFileState=static
Type=oneshot
ActiveState=inactive
SubState=dead

Id=openrazer-daemon.service
LoadState=loaded
UnitFileState=enabled
Type=dbus
ActiveState=inactive
SubState=dead

Id=broken.service
LoadState=loaded
UnitFileState=enabled
Type=simple
ActiveState=failed
SubState=failed

Id=homegraph.timer
LoadState=loaded
UnitFileState=enabled
Type=
ActiveState=active
SubState=waiting

Id=cache.timer
LoadState=loaded
UnitFileState=enabled
Type=
ActiveState=inactive
SubState=dead

Id=pulseaudio.service
LoadState=not-found
UnitFileState=
Type=
ActiveState=inactive
SubState=dead

Id=helper.service
LoadState=loaded
UnitFileState=static
Type=simple
ActiveState=inactive
SubState=dead

Id=setup.service
LoadState=loaded
UnitFileState=enabled
Type=oneshot
ActiveState=active
SubState=exited
";

    fn find<'a>(entries: &'a [Entry], name: &str) -> &'a Entry {
        entries.iter().find(|e| e.name == name).unwrap()
    }

    #[test]
    fn laufender_service_ist_running() {
        let e = parse_show(SHOW);
        let graphsync = find(&e, "logsentry-graphsync.service");
        assert_eq!(graphsync.status, Status::Running);
        assert_eq!(graphsync.kind, Kind::Service);
        assert_eq!(graphsync.detail, "active/running");
    }

    #[test]
    fn gestoppter_dauerdienst_ist_not_running_einmaliger_ist_idle() {
        let e = parse_show(SHOW);
        assert_eq!(
            find(&e, "openrazer-daemon.service").status,
            Status::NotRunning
        );
        assert_eq!(find(&e, "homegraph.service").status, Status::Idle);
        // Oneshot, der erfolgreich durchgelaufen ist und aktiv blieb.
        assert_eq!(find(&e, "setup.service").status, Status::Idle);
    }

    #[test]
    fn nicht_installierte_units_entfallen_und_nur_enabled_gilt_als_dauerdienst() {
        let e = parse_show(SHOW);
        assert!(e.iter().all(|x| x.name != "pulseaudio.service"));
        // Inaktiv, aber nicht enabled (static): nicht zum Dauerlauf gedacht.
        assert_eq!(find(&e, "helper.service").status, Status::Idle);
    }

    #[test]
    fn fehlgeschlagene_unit_ist_failed() {
        assert_eq!(
            find(&parse_show(SHOW), "broken.service").status,
            Status::Failed
        );
    }

    #[test]
    fn timer_wartend_ist_running_gestoppter_timer_not_running() {
        let e = parse_show(SHOW);
        let timer = find(&e, "homegraph.timer");
        assert_eq!(timer.kind, Kind::Timer);
        assert_eq!(timer.status, Status::Running);
        assert_eq!(find(&e, "cache.timer").status, Status::NotRunning);
    }

    #[test]
    fn leere_ausgabe_ergibt_keine_eintraege() {
        assert!(parse_show("").is_empty());
        assert!(parse_show("\n\n").is_empty());
    }

    #[test]
    fn desktop_datei_liefert_name_und_exec_ohne_feldcodes_und_ignoriert_uebersetzungen() {
        let a = parse_desktop(
            "[Desktop Entry]\nName[de]=Dampf\nName=Steam\nExec=/usr/games/steam %U\n",
        )
        .unwrap();
        assert_eq!(
            a,
            Autostart {
                name: "Steam".into(),
                exec: "/usr/games/steam".into()
            }
        );
    }

    #[test]
    fn abgeschaltete_oder_unvollstaendige_eintraege_entfallen() {
        let base = "[Desktop Entry]\nName=X\nExec=x\n";
        assert!(parse_desktop(base).is_some());
        assert!(parse_desktop(&format!("{base}Hidden=true\n")).is_none());
        assert!(parse_desktop(&format!("{base}X-GNOME-Autostart-enabled=false\n")).is_none());
        assert!(parse_desktop("[Desktop Entry]\nName=X\n").is_none());
    }

    fn procs(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn programm_wird_ueber_den_dateinamen_erkannt_auch_mit_anderem_pfad() {
        let p = procs(&[
            "/home/max/.local/share/Steam/ubuntu12_32/steam -foo",
            "bash",
        ]);
        assert!(is_running("/usr/games/steam", &p));
        assert!(!is_running("/usr/games/firefox", &p));
    }

    #[test]
    fn script_hinter_interpreter_wird_erkannt() {
        let p = procs(&["/bin/bash /home/max/scripts/opendeck/restore-monitors.sh"]);
        assert!(is_running(
            "/home/max/scripts/opendeck/restore-monitors.sh",
            &p
        ));
        assert!(!is_running(
            "/home/max/scripts/opendeck/dim-monitors.sh",
            &p
        ));
    }

    #[test]
    fn shell_wrapper_wird_nur_bei_identischer_kommandozeile_erkannt() {
        let exec = "sh -c 'sleep 3 && xdotool key super'";
        assert!(is_running(
            exec,
            &procs(&["sh -c sleep 3 && xdotool key super"])
        ));
        // Irgendeine andere Shell darf nicht als Treffer zählen.
        assert!(!is_running(
            exec,
            &procs(&["sh -c anderes", "bash", "/bin/sh"])
        ));
    }

    #[test]
    fn flatpak_start_wird_ueber_das_command_erkannt() {
        let exec = "/usr/bin/flatpak run --branch=stable --command=opendeck me.amankhanna.opendeck";
        assert!(is_running(exec, &procs(&["opendeck"])));
        assert!(!is_running(exec, &procs(&["flatpak run something"])));
    }

    #[test]
    fn programm_mit_argumenten_wird_erkannt() {
        let p = procs(&["easyeffects --gapplication-service"]);
        assert!(is_running("easyeffects --gapplication-service", &p));
    }

    #[test]
    fn autostart_laeuft_gruen_sonst_neutral_nie_rot() {
        let a = Autostart {
            name: "Steam".into(),
            exec: "/usr/games/steam".into(),
        };
        let running = autostart_entry(&a, &procs(&["/x/steam"]));
        assert_eq!(
            (running.kind, running.status),
            (Kind::Autostart, Status::Running)
        );
        assert_eq!(running.name, "Steam");
        assert_eq!(autostart_entry(&a, &procs(&["bash"])).status, Status::Idle);
    }

    #[test]
    fn filter_blendet_infrastruktur_aus_und_show_all_zeigt_alles() {
        let mk = |n: &str| Entry {
            name: n.into(),
            kind: Kind::Service,
            status: Status::Running,
            detail: String::new(),
        };
        let all = [mk("gvfs-daemon.service"), mk("logsentry-graphsync.service")];
        let hide = vec!["gvfs-".to_string()];
        let names = |v: Vec<&Entry>| v.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
        assert_eq!(
            names(visible(&all, &hide, false)),
            ["logsentry-graphsync.service"]
        );
        assert_eq!(visible(&all, &hide, true).len(), 2);
    }
}
