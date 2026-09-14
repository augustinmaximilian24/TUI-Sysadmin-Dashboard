//! JSON-Lines-Framing zwischen Daemon und Client(s).
//!
//! Eine Nachricht ist genau eine `\n`-terminierte UTF-8-Zeile. Wird
//! sowohl vom Daemon (`server.rs`/`client_task.rs`) als auch vom künftigen
//! `proto::client`-Modul genutzt -- daher hier und nicht hinter dem
//! `client`-Feature.
//!
//! Normativ: `docs/phase6-protokoll.md` Abschnitt 4, Regel 3. Eine Zeile,
//! die `max_bytes` überschreitet, bevor ein `\n` gefunden wird, führt zu
//! [`FrameError::TooLong`] -- die aufrufende Seite trennt danach die
//! Verbindung (`Goodbye(LineTooLong)` beim Daemon). Wichtig: Wir puffern
//! dafür nicht erst die komplette überlange Zeile; der interne Puffer
//! wächst nie über `max_bytes` plus höchstens eine Lesegröße des
//! zugrundeliegenden `AsyncBufRead` hinaus, bevor abgebrochen wird.

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::FrameError;

/// Liest eine Zeile aus `reader`, ohne das führende Limit `max_bytes`
/// (Länge der Nutzdaten, das trennende `\n` selbst zählt nicht mit) zu
/// überschreiten.
///
/// Rückgabewerte:
/// - `Ok(Some(zeile))`: eine vollständige, `\n`-terminierte Zeile (das
///   `\n` ist nicht enthalten), oder die letzten Bytes vor EOF, falls der
///   Sender die Verbindung ohne abschließendes `\n` beendet hat.
/// - `Ok(None)`: sauberes EOF, keine weiteren Daten.
/// - `Err(FrameError::TooLong)`: das Limit wurde überschritten; der
///   Aufrufer muss die Verbindung trennen. Die Position im Stream danach
///   ist nicht mehr sinnvoll nutzbar.
/// - `Err(FrameError::InvalidUtf8)`: eine vollständige Zeile war kein
///   gültiges UTF-8.
/// - `Err(FrameError::Io)`: I/O-Fehler beim Lesen.
pub async fn read_frame<R>(reader: &mut R, max_bytes: usize) -> Result<Option<String>, FrameError>
where
    R: AsyncBufRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            // Sauberes EOF. Ohne bisher gelesene Daten: kein weiterer Frame.
            // Mit Daten, aber ohne abschließendes \n: letzter Frame ohne
            // Terminator (z. B. Verbindung mitten in einer Zeile beendet).
            return if buf.is_empty() {
                Ok(None)
            } else {
                Ok(Some(String::from_utf8(buf)?))
            };
        }

        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            if buf.len() + pos > max_bytes {
                // Zeile ist zu lang, auch wenn das \n schon im selben
                // Lesepuffer steht. Bis einschließlich \n konsumieren,
                // damit der nächste Aufruf (falls der Aufrufer die
                // Verbindung entgegen der Empfehlung weiterliest) nicht
                // auf denselben Daten hängen bleibt.
                reader.consume(pos + 1);
                return Err(FrameError::TooLong { max_bytes });
            }
            buf.extend_from_slice(&available[..pos]);
            reader.consume(pos + 1);
            return Ok(Some(String::from_utf8(buf)?));
        }

        // Kein \n in diesem Block. Vor dem Anhängen prüfen, damit wir nie
        // mehr als max_bytes plus die aktuelle Lesegröße puffern.
        if buf.len() + available.len() > max_bytes {
            // Genug gesehen, um zu wissen, dass die Zeile zu lang ist --
            // nicht weiter puffern. Wir konsumieren den Block trotzdem,
            // damit der Reader nicht auf denselben Daten hängen bleibt,
            // falls der Aufrufer den Stream (fälschlich) weiterverwendet;
            // maßgeblich ist aber, dass er die Verbindung schließt.
            let consumed = available.len();
            reader.consume(consumed);
            return Err(FrameError::TooLong { max_bytes });
        }

        let n = available.len();
        buf.extend_from_slice(available);
        reader.consume(n);
    }
}

/// Schreibt `line` gefolgt von `\n` und flusht. `line` darf selbst kein
/// `\n` enthalten (das serialisierte JSON tut das nie); das wird hier
/// nicht geprüft, da es ausschließlich intern aus `serde_json::to_string`
/// stammt.
pub async fn write_frame<W>(writer: &mut W, line: &str) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    fn reader_for(data: &[u8]) -> BufReader<std::io::Cursor<Vec<u8>>> {
        BufReader::new(std::io::Cursor::new(data.to_vec()))
    }

    #[tokio::test]
    async fn liest_eine_einzelne_zeile() {
        let mut r = reader_for(b"hallo\n");
        assert_eq!(
            read_frame(&mut r, 1024).await.unwrap(),
            Some("hallo".to_string())
        );
        assert_eq!(read_frame(&mut r, 1024).await.unwrap(), None);
    }

    #[tokio::test]
    async fn liest_mehrere_zeilen_nacheinander() {
        let mut r = reader_for(b"eins\nzwei\ndrei\n");
        assert_eq!(
            read_frame(&mut r, 1024).await.unwrap(),
            Some("eins".to_string())
        );
        assert_eq!(
            read_frame(&mut r, 1024).await.unwrap(),
            Some("zwei".to_string())
        );
        assert_eq!(
            read_frame(&mut r, 1024).await.unwrap(),
            Some("drei".to_string())
        );
        assert_eq!(read_frame(&mut r, 1024).await.unwrap(), None);
    }

    #[tokio::test]
    async fn leere_zeile_ist_ein_gueltiger_leerer_frame() {
        let mut r = reader_for(b"\nrest\n");
        assert_eq!(read_frame(&mut r, 1024).await.unwrap(), Some(String::new()));
        assert_eq!(
            read_frame(&mut r, 1024).await.unwrap(),
            Some("rest".to_string())
        );
    }

    #[tokio::test]
    async fn zeile_exakt_an_der_grenze_ist_noch_erlaubt() {
        // max_bytes = 5, Inhalt "abcde" ist exakt 5 Byte lang.
        let mut r = reader_for(b"abcde\n");
        assert_eq!(
            read_frame(&mut r, 5).await.unwrap(),
            Some("abcde".to_string())
        );
    }

    #[tokio::test]
    async fn zeile_ein_byte_ueber_der_grenze_wird_abgelehnt() {
        // max_bytes = 5, Inhalt "abcdef" ist 6 Byte lang.
        let mut r = reader_for(b"abcdef\n");
        let err = read_frame(&mut r, 5).await.unwrap_err();
        assert!(matches!(err, FrameError::TooLong { max_bytes: 5 }));
    }

    #[tokio::test]
    async fn ueberlaenge_wird_auch_erkannt_wenn_newline_bereits_im_gepufferten_block_liegt() {
        // Regression: der erste `fill_buf`-Aufruf eines kleinen Test-Readers
        // liefert oft den gesamten Inhalt inklusive \n auf einen Schlag.
        // Die Limitprüfung darf sich nicht auf den "kein \n gefunden"-Zweig
        // verlassen, sonst rutscht eine zu lange Zeile durch, wenn sie
        // zufällig ganz in einem Lesepuffer steckt.
        let mut r = reader_for(b"zu-lange-zeile\nkuerzer\n");
        let err = read_frame(&mut r, 5).await.unwrap_err();
        assert!(matches!(err, FrameError::TooLong { max_bytes: 5 }));
    }

    #[tokio::test]
    async fn ueberlange_zeile_ohne_abschliessendes_newline_wird_ebenfalls_abgelehnt() {
        let mut r = reader_for(b"abcdefghij");
        let err = read_frame(&mut r, 3).await.unwrap_err();
        assert!(matches!(err, FrameError::TooLong { max_bytes: 3 }));
    }

    #[tokio::test]
    async fn fehlendes_newline_am_ende_liefert_die_letzte_zeile_trotzdem() {
        let mut r = reader_for(b"erste\nletzte ohne newline");
        assert_eq!(
            read_frame(&mut r, 1024).await.unwrap(),
            Some("erste".to_string())
        );
        assert_eq!(
            read_frame(&mut r, 1024).await.unwrap(),
            Some("letzte ohne newline".to_string())
        );
        assert_eq!(read_frame(&mut r, 1024).await.unwrap(), None);
    }

    #[tokio::test]
    async fn leerer_stream_liefert_sofort_none() {
        let mut r = reader_for(b"");
        assert_eq!(read_frame(&mut r, 1024).await.unwrap(), None);
    }

    #[tokio::test]
    async fn ungueltiges_utf8_wird_als_fehler_gemeldet() {
        let mut r = reader_for(&[0xff, 0xfe, b'\n']);
        let err = read_frame(&mut r, 1024).await.unwrap_err();
        assert!(matches!(err, FrameError::InvalidUtf8(_)));
    }

    #[tokio::test]
    async fn write_frame_haengt_genau_ein_newline_an() {
        let mut out: Vec<u8> = Vec::new();
        write_frame(&mut out, r#"{"type":"ping","nonce":1}"#)
            .await
            .unwrap();
        assert_eq!(out, b"{\"type\":\"ping\",\"nonce\":1}\n".to_vec());
    }

    #[tokio::test]
    async fn write_dann_read_roundtrip() {
        let mut out: Vec<u8> = Vec::new();
        write_frame(&mut out, "erste").await.unwrap();
        write_frame(&mut out, "zweite").await.unwrap();

        let mut r = reader_for(&out);
        assert_eq!(
            read_frame(&mut r, 1024).await.unwrap(),
            Some("erste".to_string())
        );
        assert_eq!(
            read_frame(&mut r, 1024).await.unwrap(),
            Some("zweite".to_string())
        );
    }
}
