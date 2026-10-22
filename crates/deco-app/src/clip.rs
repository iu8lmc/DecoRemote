//! Sincronizzazione degli appunti (solo testo) tra host e viewer.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, TryRecvError},
        Arc,
    },
    thread,
    time::Duration,
};

use tokio::sync::mpsc::UnboundedSender;

const MAX_LEN: usize = 1 << 20;

/// Controlla gli appunti locali e inoltra le modifiche a `to_peer`; applica quelle ricevute da `from_peer`.
/// Il thread termina quando `stop` è true o uno dei due canali si chiude.
pub fn spawn(to_peer: UnboundedSender<String>, from_peer: Receiver<String>, stop: Arc<AtomicBool>) {
    thread::spawn(move || {
        let Ok(mut clipboard) = arboard::Clipboard::new() else { return };
        // Il contenuto presente all'avvio non viene condiviso.
        let mut last = clipboard.get_text().unwrap_or_default();
        while !stop.load(Ordering::Relaxed) {
            loop {
                match from_peer.try_recv() {
                    Ok(text) => {
                        if text != last && clipboard.set_text(text.clone()).is_ok() {
                            last = text;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }
            if let Ok(current) = clipboard.get_text() {
                if current != last {
                    last = current.clone();
                    if current.len() <= MAX_LEN && to_peer.send(current).is_err() {
                        return;
                    }
                }
            }
            thread::sleep(Duration::from_millis(400));
        }
    });
}
