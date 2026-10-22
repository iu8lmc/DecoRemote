//! Messaggi del protocollo: handshake col server e messaggi host <-> viewer.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

/// Indirizzi UDP a cui un peer è raggiungibile (visti dal server e locali), per il collegamento diretto.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct UdpOffer {
    pub addrs: Vec<SocketAddr>,
}

/// Scelta del percorso dati, comunicata dal viewer all'host in chiaro sulla connessione relay.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathMsg {
    /// Il collegamento diretto QUIC è riuscito: l'handshake prosegue lì.
    UseQuic,
    /// Si prosegue sul relay TCP.
    UseTcp,
}

/// Primo messaggio inviato al server di rendezvous su ogni nuova connessione.
#[derive(Serialize, Deserialize, Debug)]
pub enum ClientHello {
    /// Canale di controllo di un host: registra l'ID e resta aperto.
    Host { id: String },
    /// Un viewer chiede di collegarsi all'host `target`.
    Connect { target: String, udp: Option<UdpOffer> },
    /// L'host apre una connessione dati in risposta a `ServerMsg::Incoming`.
    Accept { token: u64, udp: Option<UdpOffer> },
}

#[derive(Serialize, Deserialize, Debug)]
pub enum ServerMsg {
    Ok,
    /// Viewer e host sono collegati dal relay; `peer_udp` sono gli indirizzi UDP dell'altro lato.
    Paired { peer_udp: Option<UdpOffer> },
    Ping,
    Incoming { token: u64 },
    Error(String),
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseBtn {
    Left,
    Right,
    Middle,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Enter,
    Backspace,
    Tab,
    Escape,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Left,
    Right,
    Up,
    Down,
    Space,
    F(u8),
    Char(char),
}

/// Codec video negoziabili tra host e viewer.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    /// Un JPEG per frame: lento ma universale.
    Jpeg,
    /// AV1 a fasce indipendenti, 4:4:4 (vedi `deco-codec`).
    Av1,
}

/// Una fascia orizzontale di schermo codificata in AV1.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Av1Band {
    pub index: u16,
    /// Prima riga e altezza della fascia nello schermo.
    pub y: u32,
    pub h: u32,
    pub key: bool,
    pub data: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ScreenInfo {
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// Messaggi dall'host al viewer.
#[derive(Serialize, Deserialize, Debug)]
pub enum HostMsg {
    /// Password corretta: l'utente dell'host deve ancora consentire l'accesso.
    AwaitingApproval,
    /// L'utente dell'host ha rifiutato (o non ha risposto in tempo).
    Denied,
    /// `current` è l'indice in `screens` dello schermo che verrà trasmesso.
    Hello { host_name: String, screens: Vec<ScreenInfo>, current: u32, codecs: Vec<VideoCodec> },
    /// L'host sta ora trasmettendo lo schermo `index` (risposta a `SelectScreen`).
    ScreenChanged { index: u32 },
    Frame { width: u32, height: u32, jpeg: Vec<u8> },
    /// Solo le fasce cambiate dall'ultimo frame, ordinate per `y`.
    Av1Frame { width: u32, height: u32, bands: Vec<Av1Band> },
    Clipboard(String),
    /// Nuova forma del cursore, da usare con `Cursor { shape: id }`. RGBA non premoltiplicato.
    CursorShape { id: u64, width: u32, height: u32, hot_x: i32, hot_y: i32, rgba: Vec<u8> },
    /// Posizione (normalizzata sullo schermo trasmesso) e forma correnti del cursore dell'host.
    Cursor { x: f32, y: f32, shape: u64, visible: bool },
    Bye,
}

/// Messaggi dal viewer all'host. Le coordinate sono normalizzate in 0.0..=1.0.
#[derive(Serialize, Deserialize, Debug)]
pub enum ViewerMsg {
    MouseMove { x: f32, y: f32 },
    MouseButton { button: MouseBtn, pressed: bool, x: f32, y: f32 },
    Wheel { dx: f32, dy: f32 },
    Key { key: Key, pressed: bool },
    Mods(Mods),
    Text(String),
    Clipboard(String),
    SetQuality(u8),
    SelectScreen(u32),
    /// Scelta del codec (tra quelli annunciati nel `Hello`).
    SetCodec(VideoCodec),
    /// La decodifica ha perso il filo: l'host rimanda tutte le fasce come keyframe.
    RequestKeyframe,
    Bye,
}
