//! Configurazione persistente: ID del computer, password di accesso e server.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const DEFAULT_SERVER: &str = "15.235.240.243:21116";

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    pub id: String,
    pub password: String,
    pub server: String,
    /// Chiede conferma all'utente dell'host a ogni nuova connessione.
    #[serde(default = "default_true")]
    pub require_confirmation: bool,
    /// Accesso non vigilato: niente conferma e, chiudendo la finestra, il programma resta attivo ridotto a icona.
    #[serde(default)]
    pub unattended: bool,
}

fn default_true() -> bool {
    true
}

fn path() -> Option<PathBuf> {
    // DECOREMOTE_HOME permette di avviare più istanze sulla stessa macchina (utile per i test).
    let dir = match std::env::var_os("DECOREMOTE_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => dirs::config_dir()?.join("DecoRemote"),
    };
    Some(dir.join("config.json"))
}

pub fn random_password() -> String {
    // Niente caratteri ambigui (0/O, 1/l/I) per renderla facile da dettare.
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    (0..10)
        .map(|_| ALPHABET[rand::random_range(0..ALPHABET.len())] as char)
        .collect()
}

impl Config {
    pub fn load_or_create() -> Self {
        let loaded = path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<Config>(&s).ok());
        loaded.unwrap_or_else(|| {
            let cfg = Config {
                id: rand::random_range(100_000_000u32..1_000_000_000).to_string(),
                password: random_password(),
                server: DEFAULT_SERVER.to_string(),
                require_confirmation: true,
                unattended: false,
            };
            cfg.save();
            cfg
        })
    }

    pub fn save(&self) {
        let Some(path) = path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            if let Err(e) = std::fs::write(&path, json) {
                tracing::warn!("salvataggio config fallito: {e}");
            }
        }
    }
}
