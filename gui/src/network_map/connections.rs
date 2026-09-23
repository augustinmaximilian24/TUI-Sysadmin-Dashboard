//! Liest aktive ausgehende TCP-Verbindungen aus `/proc/net/tcp` -- ohne
//! Root-Rechte, ohne D-Bus, ohne Umweg über den privilegierten Daemon
//! (die GUI liest hier nur ihre eigene, für jeden Benutzer sichtbare
//! Sicht auf die System-Socket-Tabelle, genau wie `ss`/`netstat`; sie
//! schreibt nichts und fordert keine zusätzlichen Rechte an -- Regel 7).
//!
//! IPv6 (`/proc/net/tcp6`) ist für Phase "Netzwerk-Weltkarte" bewusst
//! außen vor: [`super::geoip`] kann nur die IPv4-Baumstruktur der
//! installierten `GeoIP.dat` lesen (siehe dortiger Kommentar). Eine
//! IPv6-Verbindung taucht hier also nicht auf, statt mit einer Adresse
//! ohne Länder-Zuordnung in der Liste zu landen.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::Path;

const PROC_NET_TCP: &str = "/proc/net/tcp";

/// Hex-Wert des `st`-Felds für `TCP_ESTABLISHED` (siehe `enum` in
/// `include/net/tcp_states.h` im Kernel).
const TCP_ESTABLISHED: &str = "01";

/// Liest die aktuell etablierten (nicht: lauschenden, nicht: wartenden)
/// TCP-Verbindungen dieses Systems und gibt deren öffentliche
/// Gegenstellen-IPv4-Adressen zurück. Kann nicht scheitern nach außen --
/// fehlt/verweigert `/proc/net/tcp` den Zugriff, ist das Ergebnis einfach
/// leer (Regel 16: kein Absturz bei einer für die Karte optionalen
/// Datenquelle).
pub fn read_established_remote_ipv4() -> Vec<Ipv4Addr> {
    std::fs::read_to_string(PROC_NET_TCP)
        .map(|contents| parse_established_remote_ipv4(&contents))
        .unwrap_or_default()
}

/// Reine Parsing-Funktion über den Text von `/proc/net/tcp`, damit sie
/// ohne Live-System testbar ist (Regel 25).
fn parse_established_remote_ipv4(contents: &str) -> Vec<Ipv4Addr> {
    contents
        .lines()
        .skip(1) // Kopfzeile ("sl local_address rem_address st ...")
        .filter_map(parse_line)
        .collect()
}

fn parse_line(line: &str) -> Option<Ipv4Addr> {
    let mut fields = line.split_whitespace();
    let _sl = fields.next()?;
    let _local = fields.next()?;
    let rem_address = fields.next()?;
    let state = fields.next()?;
    if state != TCP_ESTABLISHED {
        return None;
    }
    let (hex_ip, _hex_port) = rem_address.split_once(':')?;
    let ip = parse_hex_ipv4(hex_ip)?;
    is_routable_public(ip).then_some(ip)
}

/// `/proc/net/tcp` kodiert die Adresse als 8 Hex-Ziffern in
/// Little-Endian-Byte-Reihenfolge (Kernel schreibt das native `u32` der
/// Adresse direkt aus) -- die Bytes stehen also umgekehrt zur gewohnten
/// punktierten Dezimalschreibweise.
fn parse_hex_ipv4(hex: &str) -> Option<Ipv4Addr> {
    if hex.len() != 8 {
        return None;
    }
    let word = u32::from_str_radix(hex, 16).ok()?;
    let bytes = word.to_le_bytes();
    Some(Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]))
}

/// Ordnet jeder aktuell verbundenen öffentlichen Gegenstelle das Programm
/// zu, das die Verbindung hält -- damit die Karte nicht nur zeigt, *wohin*
/// verbunden ist, sondern auch *womit* ("Steam -> Server in den USA").
///
/// Verfahren ohne Root-Rechte (Regel 7): `/proc/net/tcp` liefert die
/// Socket-`inode` je Verbindung, `/proc/<pid>/fd/*` sind symbolische Links
/// der Form `socket:[<inode>]` -- läuft dasselbe Verfahren wie `lsof`/`ss
/// -p` intern nutzt. Nur `/proc/<pid>`-Verzeichnisse eigener Prozesse sind
/// für einen unprivilegierten Benutzer überhaupt lesbar; fremde (root-
/// oder andere Nutzer-Prozesse) liefern beim `read_dir` einen
/// Berechtigungsfehler, der hier einfach übersprungen wird (Regel 16:
/// kein Absturz, die Verbindung bleibt dann ohne Programmnamen).
pub fn resolve_program_names() -> HashMap<Ipv4Addr, String> {
    let mut result = HashMap::new();
    let Ok(contents) = std::fs::read_to_string(PROC_NET_TCP) else {
        return result;
    };
    let inode_to_ip = parse_established_inodes(&contents);
    if inode_to_ip.is_empty() {
        return result;
    }

    let Ok(proc_entries) = std::fs::read_dir("/proc") else {
        return result;
    };
    for entry in proc_entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue; // kein PID-Verzeichnis (z. B. "self", "net", "cpuinfo")
        };
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue; // fremder Prozess oder bereits beendet -- kein Fehler
        };
        for fd in fds.flatten() {
            let Ok(link) = std::fs::read_link(fd.path()) else { continue };
            let Some(inode) = parse_socket_inode(&link) else { continue };
            let Some(&ip) = inode_to_ip.get(&inode) else { continue };
            if let Some(name) = read_process_name(pid) {
                result.insert(ip, name);
            }
        }
    }
    result
}

/// Wie [`parse_established_remote_ipv4`], liefert aber zusätzlich die
/// Socket-`inode` je Verbindung (letztes Feld in `/proc/net/tcp`) statt
/// nur die Adresse -- Grundlage für [`resolve_program_names`].
fn parse_established_inodes(contents: &str) -> HashMap<u64, Ipv4Addr> {
    contents
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _sl = fields.next()?;
            let _local = fields.next()?;
            let rem_address = fields.next()?;
            let state = fields.next()?;
            if state != TCP_ESTABLISHED {
                return None;
            }
            let (hex_ip, _hex_port) = rem_address.split_once(':')?;
            let ip = parse_hex_ipv4(hex_ip)?;
            if !is_routable_public(ip) {
                return None;
            }
            let _tx_rx_queue = fields.next()?;
            let _tr_tm_when = fields.next()?;
            let _retrnsmt = fields.next()?;
            let _uid = fields.next()?;
            let _timeout = fields.next()?;
            let inode: u64 = fields.next()?.parse().ok()?;
            Some((inode, ip))
        })
        .collect()
}

/// Extrahiert die Inode-Nummer aus einem `/proc/<pid>/fd/<n>`-Symlink-Ziel
/// der Form `socket:[12345]`. Alles andere (reguläre Datei, Pipe, `anon_inode`)
/// liefert `None`.
fn parse_socket_inode(link: &Path) -> Option<u64> {
    link.to_str()?.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
}

/// Prozessname aus `/proc/<pid>/comm` -- vom Kernel selbst auf 15 Zeichen
/// gekürzt (`TASK_COMM_LEN`), reicht aber für die Anzeige ("steam",
/// "discord", "firefox").
fn read_process_name(pid: u32) -> Option<String> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let name = comm.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Nur Adressen, für die ein Länder-Lookup und ein Kartenpunkt überhaupt
/// Sinn ergeben -- lokaler/privater Verkehr (LAN, Loopback, Link-Local,
/// Multicast) hat keine sinnvolle Position auf einer Weltkarte. Auch von
/// [`super::traceroute`] genutzt, um den eigenen Router als ersten Hop
/// auszusortieren.
pub(super) fn is_routable_public(ip: Ipv4Addr) -> bool {
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feste Fixture statt eines Live-`/proc/net/tcp` (Regel 25): eine
    /// lauschende Zeile (muss ignoriert werden), eine etablierte Zeile zu
    /// einer öffentlichen Adresse, eine etablierte Zeile ins eigene LAN
    /// (muss herausgefiltert werden) und eine Zeile im `TIME_WAIT`-Zustand
    /// (muss ignoriert werden, da nicht `ESTABLISHED`).
    const FIXTURE: &str = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:9C4C 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 21520 1
   1: 0101A8C0:9C4C 04040808:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 55555 1
   2: 0101A8C0:9C4C 0101A8C0:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 55556 1
   3: 0101A8C0:9C4C 08080808:01BB 06 00000000:00000000 00:00000000 00000000  1000        0 55557 1
";

    #[test]
    fn filtert_nur_etablierte_oeffentliche_gegenstellen() {
        let result = parse_established_remote_ipv4(FIXTURE);
        assert_eq!(result, vec![Ipv4Addr::new(8, 8, 4, 4)]);
    }

    #[test]
    fn hex_adresse_wird_byte_gereiht_korrekt_gedreht() {
        // Regressionsfall mit einer asymmetrischen Adresse, damit ein
        // vertauschtes Byte-Paar nicht zufällig unbemerkt bliebe.
        assert_eq!(
            parse_hex_ipv4("0100007F"),
            Some(Ipv4Addr::new(127, 0, 0, 1))
        );
        assert_eq!(
            parse_hex_ipv4("04040808"),
            Some(Ipv4Addr::new(8, 8, 4, 4))
        );
    }

    #[test]
    fn private_und_loopback_adressen_werden_verworfen() {
        assert!(!is_routable_public(Ipv4Addr::new(192, 168, 1, 1)));
        assert!(!is_routable_public(Ipv4Addr::new(127, 0, 0, 1)));
        assert!(!is_routable_public(Ipv4Addr::new(10, 0, 0, 5)));
        assert!(is_routable_public(Ipv4Addr::new(8, 8, 8, 8)));
    }

    #[test]
    fn parse_established_inodes_liest_nur_oeffentliche_etablierte_zeilen() {
        let result = parse_established_inodes(FIXTURE);
        assert_eq!(result.len(), 1);
        assert_eq!(result.get(&55555), Some(&Ipv4Addr::new(8, 8, 4, 4)));
    }

    #[test]
    fn parse_socket_inode_erkennt_socket_symlink_ziel() {
        assert_eq!(parse_socket_inode(Path::new("socket:[55555]")), Some(55555));
        assert_eq!(parse_socket_inode(Path::new("/dev/pts/3")), None);
        assert_eq!(parse_socket_inode(Path::new("pipe:[123]")), None);
    }

}
