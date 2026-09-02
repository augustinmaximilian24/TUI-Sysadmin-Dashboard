//! Bounded Ring-Kanal für die Ingestion-Pipeline.
//!
//! Im Gegensatz zu einem normalen bounded `mpsc`-Kanal blockiert `send`
//! hier nie: Ist der Puffer voll, wird das älteste, noch nicht konsumierte
//! Element verworfen und ein Drop-Zähler erhöht (Regel 17). Der Zähler ist
//! über [`RingSender::dropped_count`] und [`RingReceiver::dropped_count`]
//! abrufbar und soll in der GUI sichtbar gemacht werden.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::{Mutex, Notify};

struct Shared<T> {
    queue: Mutex<VecDeque<T>>,
    notify: Notify,
    capacity: usize,
    dropped: AtomicU64,
    closed: AtomicBool,
    /// Anzahl noch lebender `RingSender`-Handles. Fällt sie auf 0, wird der
    /// Kanal automatisch geschlossen (wie bei `tokio::sync::mpsc`) – ohne das
    /// würde `RingReceiver::recv` nach dem letzten Sender ewig blockieren.
    sender_count: AtomicUsize,
}

/// Sende-Ende des Ring-Kanals. Klonbar; der Kanal schließt sich automatisch,
/// sobald das letzte Sender-Handle fällt (siehe [`Shared::sender_count`]).
pub struct RingSender<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Clone for RingSender<T> {
    fn clone(&self) -> Self {
        self.shared.sender_count.fetch_add(1, Ordering::AcqRel);
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> Drop for RingSender<T> {
    fn drop(&mut self) {
        if self.shared.sender_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.shared.closed.store(true, Ordering::Release);
            self.shared.notify.notify_waiters();
        }
    }
}

/// Empfangs-Ende des Ring-Kanals. Nicht klonbar: es gibt genau einen Konsumenten.
pub struct RingReceiver<T> {
    shared: Arc<Shared<T>>,
}

/// Erstellt ein neues Ring-Kanal-Paar mit fester Kapazität (Regel 18: harte
/// Obergrenze, kein unbeschränktes Wachstum).
///
/// # Panics
/// Wenn `capacity == 0` übergeben wird, da ein Kanal ohne Platz sinnlos ist.
pub fn ring_channel<T>(capacity: usize) -> (RingSender<T>, RingReceiver<T>) {
    assert!(capacity > 0, "Ring-Kanal-Kapazität muss größer als 0 sein");
    let shared = Arc::new(Shared {
        queue: Mutex::new(VecDeque::with_capacity(capacity)),
        notify: Notify::new(),
        capacity,
        dropped: AtomicU64::new(0),
        closed: AtomicBool::new(false),
        sender_count: AtomicUsize::new(1),
    });
    (
        RingSender {
            shared: Arc::clone(&shared),
        },
        RingReceiver { shared },
    )
}

impl<T> RingSender<T> {
    /// Fügt ein Element hinzu. Ist der Puffer voll, wird das älteste Element
    /// verworfen (Regel 17) und der Drop-Zähler erhöht.
    pub async fn send(&self, value: T) {
        let mut queue = self.shared.queue.lock().await;
        if queue.len() >= self.shared.capacity {
            queue.pop_front();
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
        }
        queue.push_back(value);
        drop(queue);
        self.shared.notify.notify_one();
    }

    /// Anzahl der bisher wegen Überlauf verworfenen Elemente.
    pub fn dropped_count(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// Schließt den Kanal explizit, auch wenn noch weitere `RingSender`-Klone
    /// existieren. Bereits im Puffer liegende Elemente können danach noch
    /// abgeholt werden. Für den Normalfall (letzter Sender fällt) reicht
    /// bereits das automatische Schließen über `Drop`.
    pub fn close(&self) {
        self.shared.closed.store(true, Ordering::Relaxed);
        self.shared.notify.notify_waiters();
    }
}

impl<T> RingReceiver<T> {
    /// Wartet auf das nächste Element. Liefert `None`, sobald der Kanal
    /// geschlossen wurde und keine gepufferten Elemente mehr vorliegen.
    pub async fn recv(&mut self) -> Option<T> {
        loop {
            {
                let mut queue = self.shared.queue.lock().await;
                if let Some(value) = queue.pop_front() {
                    return Some(value);
                }
                if self.shared.closed.load(Ordering::Relaxed) {
                    return None;
                }
            }
            self.shared.notify.notified().await;
        }
    }

    /// Anzahl der bisher wegen Überlauf verworfenen Elemente.
    pub fn dropped_count(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn send_und_recv_ohne_ueberlauf() {
        let (tx, mut rx) = ring_channel::<u32>(4);
        tx.send(1).await;
        tx.send(2).await;
        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(rx.recv().await, Some(2));
        assert_eq!(tx.dropped_count(), 0);
    }

    #[tokio::test]
    async fn ueberlauf_verwirft_aeltestes_element() {
        let (tx, mut rx) = ring_channel::<u32>(2);
        tx.send(1).await;
        tx.send(2).await;
        // Puffer ist voll (Kapazität 2) -> das älteste Element (1) wird verworfen.
        tx.send(3).await;

        assert_eq!(tx.dropped_count(), 1);
        assert_eq!(rx.recv().await, Some(2));
        assert_eq!(rx.recv().await, Some(3));
    }

    #[tokio::test]
    async fn close_beendet_recv_nach_leerung() {
        let (tx, mut rx) = ring_channel::<u32>(4);
        tx.send(42).await;
        tx.close();
        assert_eq!(rx.recv().await, Some(42));
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn dropped_count_ist_ueber_beide_enden_konsistent() {
        let (tx, mut rx) = ring_channel::<u32>(1);
        tx.send(1).await;
        tx.send(2).await;
        assert_eq!(rx.dropped_count(), 1);
        rx.recv().await;
    }

    #[test]
    #[should_panic(expected = "größer als 0")]
    fn kapazitaet_null_paniked_bewusst_bei_konstruktion() {
        let _ = ring_channel::<u32>(0);
    }
}
