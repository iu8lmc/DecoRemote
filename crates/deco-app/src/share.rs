//! Chat e trasferimento file fra host e viewer, sopra la stessa sessione cifrata.
//!
//! Lo stesso motore gira su entrambi i lati (`Role`): l'interfaccia lo comanda con `Share` e ne legge lo
//! stato in `View`; i messaggi in uscita escono da un canale che la sessione inoltra al peer. Il viewer
//! può inviare file all'host (finiscono nella cartella di scambio) e chiederne all'host (scaricamento,
//! anche sfogliando le cartelle remote o cercando i log ADIF); l'host non scrive mai fuori dalla cartella
//! di scambio e il viewer solo in quella dei file scaricati.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, UNIX_EPOCH},
};

use deco_proto::msg::{DirEntry, ShareMsg};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
};

/// Dimensione di un pezzo di file.
pub const CHUNK: usize = 32 * 1024;
/// Byte che si possono avere in volo senza conferma.
const WINDOW: u64 = 8 * CHUNK as u64;
pub const MAX_CHAT: usize = 2000;
const MAX_CHAT_LINES: usize = 300;
const MAX_FILE: u64 = 2 * 1024 * 1024 * 1024;
const MAX_LIST: usize = 2000;
const ADIF_SCAN_LIMIT: Duration = Duration::from_secs(6);
pub const ADIF_PATH: &str = "adif:";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// Riceve i file del viewer nella cartella di scambio e serve le richieste di scaricamento.
    Host,
    /// Accetta solo i file che ha chiesto.
    Viewer,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Waiting,
    Running,
    Done,
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct Transfer {
    pub key: u64,
    pub name: String,
    pub size: u64,
    pub done: u64,
    pub upload: bool,
    pub status: Status,
    /// File finale sul disco locale (ricevuto) o sorgente (inviato).
    pub path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct ChatLine {
    pub mine: bool,
    pub text: String,
}

#[derive(Clone, Debug, Default)]
pub struct Listing {
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<DirEntry>,
    pub error: Option<String>,
}

/// Stato visibile all'interfaccia.
#[derive(Default)]
pub struct View {
    pub chat: Vec<ChatLine>,
    /// Messaggi arrivati da quando la chat è stata guardata l'ultima volta.
    pub unread: usize,
    pub transfers: Vec<Transfer>,
    pub listing: Option<Listing>,
    pub listing_busy: bool,
}

pub enum Cmd {
    Chat(String),
    Upload(PathBuf),
    Download { path: String, name: String },
    List(String),
    Cancel(u64),
}

enum Input {
    Cmd(Cmd),
    Msg(ShareMsg),
}

/// Maniglia usata da interfaccia e sessione. Si clona liberamente.
#[derive(Clone)]
pub struct Share {
    pub view: Arc<Mutex<View>>,
    tx: mpsc::UnboundedSender<Input>,
    /// Cartella in cui finiscono i file ricevuti.
    pub recv_dir: PathBuf,
}

impl Share {
    pub fn chat(&self, text: &str) {
        let _ = self.tx.send(Input::Cmd(Cmd::Chat(text.to_string())));
    }

    pub fn upload(&self, path: PathBuf) {
        let _ = self.tx.send(Input::Cmd(Cmd::Upload(path)));
    }

    pub fn download(&self, path: &str, name: &str) {
        let _ = self.tx.send(Input::Cmd(Cmd::Download { path: path.to_string(), name: name.to_string() }));
    }

    pub fn list(&self, path: &str) {
        let _ = self.tx.send(Input::Cmd(Cmd::List(path.to_string())));
    }

    pub fn cancel(&self, key: u64) {
        let _ = self.tx.send(Input::Cmd(Cmd::Cancel(key)));
    }

    /// Messaggio arrivato dal peer.
    pub fn incoming(&self, msg: ShareMsg) {
        let _ = self.tx.send(Input::Msg(msg));
    }

    /// Azzera il contatore dei messaggi non letti.
    pub fn mark_read(&self) {
        self.view.lock().unwrap().unread = 0;
    }
}

/// Avvia il motore sul runtime corrente. Restituisce la maniglia e i messaggi da inoltrare al peer.
/// `on_event` viene chiamata a ogni novità per l'interfaccia (ridisegno); `on_chat` a ogni messaggio ricevuto.
pub fn start(
    role: Role,
    recv_dir: PathBuf,
    on_event: impl Fn() + Send + Sync + 'static,
    on_chat: impl Fn() + Send + Sync + 'static,
) -> (Share, mpsc::UnboundedReceiver<ShareMsg>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let share = Share { view: Arc::default(), tx, recv_dir: recv_dir.clone() };
    let engine = Engine {
        role,
        view: share.view.clone(),
        out: out_tx,
        recv_dir,
        outgoing: HashMap::new(),
        incoming: HashMap::new(),
        wanted: HashMap::new(),
        next_id: 1,
        next_key: 1,
        on_event: Box::new(on_event),
        on_chat: Box::new(on_chat),
    };
    tokio::spawn(engine.run(rx));
    (share, out_rx)
}

struct Outgoing {
    key: u64,
    file: tokio::fs::File,
    size: u64,
    sent: u64,
    acked: u64,
    accepted: bool,
    finished: bool,
}

struct Incoming {
    key: u64,
    file: tokio::fs::File,
    part: PathBuf,
    dest: PathBuf,
    size: u64,
    received: u64,
}

struct Engine {
    role: Role,
    view: Arc<Mutex<View>>,
    out: mpsc::UnboundedSender<ShareMsg>,
    recv_dir: PathBuf,
    /// File che stiamo inviando, per `id` assegnato da noi.
    outgoing: HashMap<u32, Outgoing>,
    /// File che stiamo ricevendo, per `id` assegnato dal peer.
    incoming: HashMap<u32, Incoming>,
    /// Scaricamenti chiesti e non ancora offerti: id della richiesta -> chiave del trasferimento.
    wanted: HashMap<u32, u64>,
    next_id: u32,
    next_key: u64,
    on_event: Box<dyn Fn() + Send + Sync>,
    on_chat: Box<dyn Fn() + Send + Sync>,
}

impl Engine {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Input>) {
        while let Some(input) = rx.recv().await {
            match input {
                Input::Cmd(cmd) => self.command(cmd).await,
                Input::Msg(msg) => self.message(msg).await,
            }
            self.pump().await;
            (self.on_event)();
        }
        // Sessione finita: i file a metà non servono a nessuno.
        for (_, inc) in self.incoming.drain() {
            drop(inc.file);
            let _ = tokio::fs::remove_file(&inc.part).await;
        }
        let mut view = self.view.lock().unwrap();
        for t in view.transfers.iter_mut().filter(|t| matches!(t.status, Status::Waiting | Status::Running)) {
            t.status = Status::Failed("sessione chiusa".into());
        }
        drop(view);
        (self.on_event)();
    }

    fn send(&self, msg: ShareMsg) {
        let _ = self.out.send(msg);
    }

    fn new_key(&mut self) -> u64 {
        self.next_key += 1;
        self.next_key
    }

    fn new_id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    fn update(&self, key: u64, f: impl FnOnce(&mut Transfer)) {
        if let Some(t) = self.view.lock().unwrap().transfers.iter_mut().find(|t| t.key == key) {
            f(t);
        }
    }

    fn fail(&self, key: u64, reason: impl Into<String>) {
        let reason = reason.into();
        self.update(key, |t| t.status = Status::Failed(reason));
    }

    async fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Chat(text) => {
                let text: String = text.trim().chars().take(MAX_CHAT).collect();
                if text.is_empty() {
                    return;
                }
                push_chat(&self.view, true, text.clone());
                self.send(ShareMsg::Chat { text });
            }
            Cmd::Upload(path) => self.upload(path).await,
            Cmd::Download { path, name } => {
                let (id, key) = (self.new_id(), self.new_key());
                self.wanted.insert(id, key);
                self.view.lock().unwrap().transfers.push(Transfer {
                    key,
                    name,
                    size: 0,
                    done: 0,
                    upload: false,
                    status: Status::Waiting,
                    path: None,
                });
                self.send(ShareMsg::FileRequest { id, path });
            }
            Cmd::List(path) => {
                self.view.lock().unwrap().listing_busy = true;
                self.send(ShareMsg::ListDir { path });
            }
            Cmd::Cancel(key) => {
                if let Some((&id, _)) = self.outgoing.iter().find(|(_, o)| o.key == key) {
                    self.outgoing.remove(&id);
                    self.send(ShareMsg::FileAbort { id });
                } else if let Some((&id, _)) = self.incoming.iter().find(|(_, i)| i.key == key) {
                    if let Some(inc) = self.incoming.remove(&id) {
                        drop(inc.file);
                        let _ = tokio::fs::remove_file(&inc.part).await;
                    }
                    self.send(ShareMsg::FileStop { id, reason: "annullato".into() });
                }
                self.fail(key, "annullato");
            }
        }
    }

    async fn upload(&mut self, path: PathBuf) {
        let key = self.new_key();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
        let mut transfer = Transfer {
            key,
            name: name.clone(),
            size: 0,
            done: 0,
            upload: true,
            status: Status::Waiting,
            path: Some(path.clone()),
        };
        let opened = async {
            let meta = tokio::fs::metadata(&path).await.map_err(|e| e.to_string())?;
            if !meta.is_file() {
                return Err("non è un file".to_string());
            }
            if meta.len() > MAX_FILE {
                return Err("file troppo grande (massimo 2 GB)".to_string());
            }
            let file = tokio::fs::File::open(&path).await.map_err(|e| e.to_string())?;
            Ok((file, meta.len()))
        }
        .await;
        match opened {
            Ok((file, size)) => {
                transfer.size = size;
                let id = self.new_id();
                self.outgoing.insert(id, Outgoing { key, file, size, sent: 0, acked: 0, accepted: false, finished: false });
                self.view.lock().unwrap().transfers.push(transfer);
                self.send(ShareMsg::FileOffer { id, name, size, request: None });
            }
            Err(e) => {
                transfer.status = Status::Failed(e);
                self.view.lock().unwrap().transfers.push(transfer);
            }
        }
    }

    async fn message(&mut self, msg: ShareMsg) {
        match msg {
            ShareMsg::Chat { text } => {
                let text: String = text.chars().take(MAX_CHAT).collect();
                if !text.trim().is_empty() {
                    push_chat(&self.view, false, text);
                    (self.on_chat)();
                }
            }
            ShareMsg::FileOffer { id, name, size, request } => self.offered(id, name, size, request).await,
            ShareMsg::FileAccept { id } => {
                if let Some(o) = self.outgoing.get_mut(&id) {
                    o.accepted = true;
                    let key = o.key;
                    self.update(key, |t| t.status = Status::Running);
                }
            }
            ShareMsg::FileChunk { id, data } => self.chunk(id, data).await,
            ShareMsg::FileDone { id } => self.finish(id).await,
            ShareMsg::FileAbort { id } => {
                if let Some(inc) = self.incoming.remove(&id) {
                    drop(inc.file);
                    let _ = tokio::fs::remove_file(&inc.part).await;
                    self.fail(inc.key, "interrotto dall'altro lato");
                }
            }
            ShareMsg::FileStop { id, reason } => {
                if let Some(o) = self.outgoing.remove(&id) {
                    self.fail(o.key, format!("rifiutato: {reason}"));
                } else if let Some(key) = self.wanted.remove(&id) {
                    self.fail(key, reason);
                }
            }
            ShareMsg::FileAck { id, received } => {
                let Some(o) = self.outgoing.get_mut(&id) else { return };
                o.acked = o.acked.max(received.min(o.size));
                let (key, acked, complete) = (o.key, o.acked, o.finished && o.acked >= o.size);
                self.update(key, |t| t.done = acked);
                if complete {
                    self.outgoing.remove(&id);
                    self.update(key, |t| t.status = Status::Done);
                }
            }
            ShareMsg::FileRequest { id, path } => self.serve_request(id, path).await,
            ShareMsg::ListDir { path } => {
                if self.role != Role::Host {
                    return;
                }
                let (out, recv_dir) = (self.out.clone(), self.recv_dir.clone());
                tokio::spawn(async move {
                    let msg = tokio::task::spawn_blocking(move || list_dir(&path, &recv_dir))
                        .await
                        .unwrap_or_else(|_| ShareMsg::DirList {
                            path: String::new(),
                            parent: None,
                            entries: Vec::new(),
                            error: Some("elenco non disponibile".into()),
                        });
                    let _ = out.send(msg);
                });
            }
            ShareMsg::DirList { path, parent, entries, error } => {
                let mut view = self.view.lock().unwrap();
                view.listing = Some(Listing { path, parent, entries, error });
                view.listing_busy = false;
            }
        }
    }

    /// Il peer propone un file.
    async fn offered(&mut self, id: u32, name: String, size: u64, request: Option<u32>) {
        let refuse = |this: &Self, reason: &str| this.send(ShareMsg::FileStop { id, reason: reason.into() });
        if size > MAX_FILE {
            return refuse(self, "file troppo grande (massimo 2 GB)");
        }
        let key = match (self.role, request) {
            // L'host riceve solo ciò che il viewer gli manda di sua iniziativa.
            (Role::Host, None) => {
                let key = self.new_key();
                self.view.lock().unwrap().transfers.push(Transfer {
                    key,
                    name: name.clone(),
                    size,
                    done: 0,
                    upload: false,
                    status: Status::Waiting,
                    path: None,
                });
                key
            }
            // Il viewer accetta solo ciò che ha chiesto.
            (Role::Viewer, Some(req)) => match self.wanted.remove(&req) {
                Some(key) => key,
                None => return refuse(self, "non richiesto"),
            },
            _ => return refuse(self, "non consentito"),
        };
        let dest = unique_path(&self.recv_dir, &sanitize_name(&name));
        let part = part_path(&dest);
        let file = match prepare_part(&self.recv_dir, &part).await {
            Ok(f) => f,
            Err(e) => {
                self.fail(key, format!("impossibile creare il file: {e}"));
                return refuse(self, "impossibile creare il file");
            }
        };
        let shown = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(name);
        self.update(key, |t| {
            t.name = shown;
            t.size = size;
            t.status = Status::Running;
            t.path = Some(dest.clone());
        });
        self.incoming.insert(id, Incoming { key, file, part, dest, size, received: 0 });
        self.send(ShareMsg::FileAccept { id });
        if size == 0 {
            self.finish(id).await;
        }
    }

    async fn chunk(&mut self, id: u32, data: Vec<u8>) {
        let Some(inc) = self.incoming.get_mut(&id) else { return };
        inc.received += data.len() as u64;
        let write = if inc.received > inc.size { Err("più dati del previsto".to_string()) } else {
            inc.file.write_all(&data).await.map_err(|e| e.to_string())
        };
        match write {
            Ok(()) => {
                let (key, received) = (inc.key, inc.received);
                self.update(key, |t| t.done = received);
                self.send(ShareMsg::FileAck { id, received });
            }
            Err(e) => {
                if let Some(inc) = self.incoming.remove(&id) {
                    drop(inc.file);
                    let _ = tokio::fs::remove_file(&inc.part).await;
                    self.fail(inc.key, e);
                }
                self.send(ShareMsg::FileStop { id, reason: "errore di scrittura".into() });
            }
        }
    }

    async fn finish(&mut self, id: u32) {
        let Some(mut inc) = self.incoming.remove(&id) else { return };
        let result = async {
            if inc.received != inc.size {
                return Err("file incompleto".to_string());
            }
            inc.file.flush().await.map_err(|e| e.to_string())?;
            drop(inc.file);
            tokio::fs::rename(&inc.part, &inc.dest).await.map_err(|e| e.to_string())
        }
        .await;
        match result {
            Ok(()) => self.update(inc.key, |t| {
                t.done = t.size;
                t.status = Status::Done;
            }),
            Err(e) => {
                let _ = tokio::fs::remove_file(&inc.part).await;
                self.fail(inc.key, e);
            }
        }
    }

    /// Il viewer chiede un file: l'host lo offre come un normale invio.
    async fn serve_request(&mut self, req: u32, path: String) {
        if self.role != Role::Host {
            return;
        }
        let stop = |this: &Self, reason: &str| this.send(ShareMsg::FileStop { id: req, reason: reason.into() });
        let path = PathBuf::from(path);
        let opened = async {
            let meta = tokio::fs::metadata(&path).await.map_err(|e| e.to_string())?;
            if !meta.is_file() {
                return Err("non è un file".to_string());
            }
            if meta.len() > MAX_FILE {
                return Err("file troppo grande (massimo 2 GB)".to_string());
            }
            let file = tokio::fs::File::open(&path).await.map_err(|e| e.to_string())?;
            Ok((file, meta.len()))
        }
        .await;
        match opened {
            Err(e) => stop(self, &e),
            Ok((file, size)) => {
                let (id, key) = (self.new_id(), self.new_key());
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
                self.view.lock().unwrap().transfers.push(Transfer {
                    key,
                    name: name.clone(),
                    size,
                    done: 0,
                    upload: true,
                    status: Status::Waiting,
                    path: Some(path),
                });
                self.outgoing.insert(id, Outgoing { key, file, size, sent: 0, acked: 0, accepted: false, finished: false });
                self.send(ShareMsg::FileOffer { id, name, size, request: Some(req) });
            }
        }
    }

    /// Spedisce pezzi ai file accettati finché la finestra di controllo di flusso lo consente.
    async fn pump(&mut self) {
        let ids: Vec<u32> = self.outgoing.keys().copied().collect();
        for id in ids {
            let mut failed = None;
            if let Some(o) = self.outgoing.get_mut(&id) {
                while o.accepted && !o.finished && o.sent - o.acked < WINDOW {
                    let want = CHUNK.min((o.size - o.sent) as usize);
                    let mut data = vec![0u8; want];
                    if want > 0 {
                        if let Err(e) = o.file.read_exact(&mut data).await {
                            failed = Some(e.to_string());
                            break;
                        }
                        o.sent += want as u64;
                        let _ = self.out.send(ShareMsg::FileChunk { id, data });
                    }
                    if o.sent >= o.size {
                        o.finished = true;
                        let _ = self.out.send(ShareMsg::FileDone { id });
                    }
                }
                if o.finished && o.acked >= o.size {
                    let key = o.key;
                    self.outgoing.remove(&id);
                    self.update(key, |t| t.status = Status::Done);
                    continue;
                }
            }
            if let Some(e) = failed {
                if let Some(o) = self.outgoing.remove(&id) {
                    self.fail(o.key, format!("lettura fallita: {e}"));
                    self.send(ShareMsg::FileAbort { id });
                }
            }
        }
    }
}

fn push_chat(view: &Mutex<View>, mine: bool, text: String) {
    let mut view = view.lock().unwrap();
    view.chat.push(ChatLine { mine, text });
    if view.chat.len() > MAX_CHAT_LINES {
        view.chat.remove(0);
    }
    if !mine {
        view.unread += 1;
    }
}

// ---------------------------------------------------------------------------------------- file system

/// Nome sicuro per un file ricevuto: niente percorsi, caratteri vietati o nomi riservati di Windows.
pub fn sanitize_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mut clean: String = base
        .chars()
        .map(|c| if c.is_control() || "<>:\"|?*".contains(c) { '_' } else { c })
        .collect();
    clean = clean.trim_matches(|c: char| c == '.' || c == ' ').to_string();
    if clean.chars().count() > 120 {
        let ext = Path::new(&clean).extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
        let ext = if ext.chars().count() <= 10 { format!(".{ext}") } else { String::new() };
        let stem: String = clean.chars().take(100).collect();
        clean = format!("{stem}{ext}");
    }
    let stem = clean.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT")) && stem.len() == 4 && stem.as_bytes()[3].is_ascii_digit());
    if clean.is_empty() || reserved {
        clean = format!("file_{clean}");
    }
    clean
}

/// `dir/name`, oppure `dir/name (1).ext`, `(2)`… se esiste già (anche come file parziale).
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() && !part_path(&candidate).exists() {
        return candidate;
    }
    let path = Path::new(name);
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists() && !part_path(p).exists())
        .expect("sequenza infinita")
}

fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".part");
    dest.with_file_name(name)
}

async fn prepare_part(dir: &Path, part: &Path) -> std::io::Result<tokio::fs::File> {
    tokio::fs::create_dir_all(dir).await?;
    tokio::fs::OpenOptions::new().write(true).create_new(true).open(part).await
}

/// Cartella in cui finiscono i file ricevuti: `Documenti\DecoRemote\<nome>`.
pub fn default_recv_dir(name: &str) -> PathBuf {
    dirs::document_dir().or_else(dirs::home_dir).unwrap_or_else(std::env::temp_dir).join("DecoRemote").join(name)
}

fn is_adif(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("adi") || e.eq_ignore_ascii_case("adif"))
}

fn entry_of(path: &Path, name: Option<String>, meta: &std::fs::Metadata) -> DirEntry {
    DirEntry {
        name: name.unwrap_or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
        path: path.to_string_lossy().into_owned(),
        is_dir: meta.is_dir(),
        size: if meta.is_dir() { 0 } else { meta.len() },
        modified: meta.modified().ok().and_then(|m| m.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs()),
    }
}

/// Contenuto di `path` per il viewer: "" = punti di partenza, `adif:` = ricerca dei log ADIF.
pub fn list_dir(path: &str, recv_dir: &Path) -> ShareMsg {
    let err = |path: &str, e: String| ShareMsg::DirList {
        path: path.to_string(),
        parent: Some(String::new()),
        entries: Vec::new(),
        error: Some(e),
    };
    match path {
        "" => {
            let mut entries = Vec::new();
            let mut add = |label: &str, p: Option<PathBuf>| {
                if let Some(p) = p.filter(|p| p.is_dir()) {
                    if let Ok(meta) = std::fs::metadata(&p) {
                        entries.push(entry_of(&p, Some(label.to_string()), &meta));
                    }
                }
            };
            add("Cartella di scambio", Some(recv_dir.to_path_buf()));
            add("Documenti", dirs::document_dir());
            add("Desktop", dirs::desktop_dir());
            add("Download", dirs::download_dir());
            add("Cartella utente", dirs::home_dir());
            for letter in b'C'..=b'Z' {
                let root = PathBuf::from(format!("{}:\\", letter as char));
                if root.is_dir() {
                    add(&format!("Disco {}:", letter as char), Some(root));
                }
            }
            ShareMsg::DirList { path: String::new(), parent: None, entries, error: None }
        }
        ADIF_PATH => ShareMsg::DirList {
            path: ADIF_PATH.into(),
            parent: Some(String::new()),
            entries: find_adif(recv_dir),
            error: None,
        },
        _ => {
            let dir = Path::new(path);
            let read = match std::fs::read_dir(dir) {
                Ok(r) => r,
                Err(e) => return err(path, e.to_string()),
            };
            let mut entries: Vec<DirEntry> = read
                .filter_map(|e| e.ok())
                .filter_map(|e| e.metadata().ok().map(|m| entry_of(&e.path(), None, &m)))
                .take(MAX_LIST)
                .collect();
            entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
            let parent = Some(dir.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default());
            ShareMsg::DirList { path: path.to_string(), parent, entries, error: None }
        }
    }
}

/// Cerca i file `.adi`/`.adif` nei posti dove di solito stanno i log (WSJT-X, JTDX, Decodium…), dal più recente.
pub fn find_adif(recv_dir: &Path) -> Vec<DirEntry> {
    let deadline = Instant::now() + ADIF_SCAN_LIMIT;
    let mut roots: Vec<(PathBuf, usize)> = vec![(recv_dir.to_path_buf(), 2)];
    for (dir, depth) in [
        (dirs::document_dir(), 3),
        (dirs::desktop_dir(), 2),
        (dirs::download_dir(), 2),
        (dirs::data_local_dir(), 3),
        (dirs::data_dir(), 3),
        (dirs::home_dir(), 1),
    ] {
        if let Some(d) = dir {
            roots.push((d, depth));
        }
    }
    let mut found: HashMap<PathBuf, DirEntry> = HashMap::new();
    for (root, depth) in roots {
        scan_adif(&root, depth, deadline, &mut found);
    }
    let mut entries: Vec<DirEntry> = found.into_values().collect();
    entries.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.name.cmp(&b.name)));
    entries.truncate(300);
    entries
}

fn scan_adif(dir: &Path, depth: usize, deadline: Instant, found: &mut HashMap<PathBuf, DirEntry>) {
    if Instant::now() > deadline || found.len() >= 600 {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else { return };
    for entry in read.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            // Cartelle che non contengono log e rallentano la ricerca.
            let skip = name.starts_with('.')
                || matches!(
                    name.as_str(),
                    "temp" | "tmp" | "cache" | "caches" | "code cache" | "gpucache" | "packages" | "node_modules"
                        | "crashdumps" | "microsoft" | "google" | "mozilla" | "jetbrains" | "nvidia" | "npm-cache"
                );
            if depth > 0 && !skip && !meta.file_type().is_symlink() {
                scan_adif(&path, depth - 1, deadline, found);
            }
        } else if is_adif(&path) {
            found.entry(path.clone()).or_insert_with(|| entry_of(&path, None, &meta));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_sanitized() {
        assert_eq!(sanitize_name("..\\..\\Windows\\evil.exe"), "evil.exe");
        assert_eq!(sanitize_name("a/b/log.adi"), "log.adi");
        assert_eq!(sanitize_name("co:n?.txt"), "co_n_.txt");
        assert_eq!(sanitize_name("CON.txt"), "file_CON.txt");
        assert_eq!(sanitize_name("  .. "), "file_");
        assert!(sanitize_name(&"x".repeat(500)).chars().count() <= 120);
    }

    #[test]
    fn unique_names_do_not_overwrite() {
        let dir = std::env::temp_dir().join(format!("decoremote-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("log.adi"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "log.adi"), dir.join("log (1).adi"));
        assert_eq!(unique_path(&dir, "altro.adi"), dir.join("altro.adi"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Due motori collegati fra loro: chat, invio, scaricamento, elenco cartelle e rifiuti.
    #[tokio::test]
    async fn end_to_end_between_two_engines() {
        let base = std::env::temp_dir().join(format!("decoremote-e2e-{}", std::process::id()));
        let (host_dir, viewer_dir, src) = (base.join("host"), base.join("viewer"), base.join("src"));
        std::fs::create_dir_all(&src).unwrap();
        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(src.join("grande.adi"), &big).unwrap();
        std::fs::write(src.join("vuoto.adif"), b"").unwrap();

        let (host, mut host_out) = start(Role::Host, host_dir.clone(), || {}, || {});
        let (viewer, mut viewer_out) = start(Role::Viewer, viewer_dir.clone(), || {}, || {});
        // Cavi fra i due motori.
        let (h, v) = (host.clone(), viewer.clone());
        tokio::spawn(async move {
            while let Some(m) = host_out.recv().await {
                v.incoming(m);
            }
        });
        let (h2, _) = (h.clone(), ());
        tokio::spawn(async move {
            while let Some(m) = viewer_out.recv().await {
                h2.incoming(m);
            }
        });

        async fn until(what: &str, mut cond: impl FnMut() -> bool) {
            for _ in 0..200 {
                if cond() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("timeout: {what}");
        }

        // Chat in entrambe le direzioni.
        viewer.chat("ciao host");
        host.chat("ciao viewer");
        until("chat", || host.view.lock().unwrap().chat.len() == 2 && viewer.view.lock().unwrap().chat.len() == 2).await;
        assert_eq!(host.view.lock().unwrap().unread, 1);
        assert!(host.view.lock().unwrap().chat.iter().any(|l| !l.mine && l.text == "ciao host"));

        // Il viewer invia un file (più pezzi) e uno vuoto all'host.
        viewer.upload(src.join("grande.adi"));
        viewer.upload(src.join("vuoto.adif"));
        until("upload", || viewer.view.lock().unwrap().transfers.iter().all(|t| t.status == Status::Done)).await;
        until("host done", || {
            let v = host.view.lock().unwrap();
            v.transfers.len() == 2 && v.transfers.iter().all(|t| t.status == Status::Done)
        })
        .await;
        assert_eq!(std::fs::read(host_dir.join("grande.adi")).unwrap(), big);
        assert_eq!(std::fs::read(host_dir.join("vuoto.adif")).unwrap().len(), 0);
        assert!(!host_dir.join("grande.adi.part").exists());

        // Elenco della cartella dell'host e scaricamento.
        viewer.list(&host_dir.to_string_lossy());
        until("listing", || viewer.view.lock().unwrap().listing.as_ref().is_some_and(|l| l.entries.len() == 2)).await;
        viewer.download(&host_dir.join("grande.adi").to_string_lossy(), "grande.adi");
        until("download", || {
            let v = viewer.view.lock().unwrap();
            v.transfers.iter().any(|t| !t.upload && t.status == Status::Done)
        })
        .await;
        assert_eq!(std::fs::read(viewer_dir.join("grande.adi")).unwrap(), big);

        // File inesistente: lo scaricamento fallisce con un motivo.
        viewer.download(&host_dir.join("manca.adi").to_string_lossy(), "manca.adi");
        until("missing", || {
            let v = viewer.view.lock().unwrap();
            v.transfers.iter().any(|t| t.name == "manca.adi" && matches!(t.status, Status::Failed(_)))
        })
        .await;

        // L'host non può spedire file di sua iniziativa al viewer.
        host.upload(src.join("grande.adi"));
        until("refused", || host.view.lock().unwrap().transfers.iter().any(|t| matches!(t.status, Status::Failed(_)))).await;
        assert!(!viewer_dir.join("grande (1).adi").exists());

        std::fs::remove_dir_all(&base).unwrap();
    }
}
