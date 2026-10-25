//! Interfaccia di chat e trasferimento file (vedi `share`): pannello del viewer e riquadro dell'host.

use std::{
    path::PathBuf,
    sync::mpsc::{channel, Receiver, Sender},
};

use egui::{Color32, RichText};

use crate::share::{Share, Status, Transfer, ADIF_PATH};

const ACCENT: Color32 = Color32::from_rgb(0x2d, 0xb8, 0xa8);
const OK: Color32 = Color32::from_rgb(0x4c, 0xc2, 0x6a);
const WARN: Color32 = Color32::from_rgb(0xf0, 0xb4, 0x3c);
const BAD: Color32 = Color32::from_rgb(0xe5, 0x5a, 0x5a);

const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp", "tif", "tiff"];
const ADIF_EXTS: &[&str] = &["adi", "adif"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Chat,
    Files,
}

/// Stato dell'interfaccia di chat e file (testo digitato, scheda, finestre di scelta file aperte).
pub struct SharePanel {
    tab: Tab,
    input: String,
    only_adif: bool,
    /// Elenco delle cartelle remote già richiesto per questa sessione.
    browsed: bool,
    picked_tx: Sender<Vec<PathBuf>>,
    picked_rx: Receiver<Vec<PathBuf>>,
    picking: bool,
    /// Il campo di testo della chat ha il focus: la tastiera non va inoltrata all'host.
    pub typing: bool,
}

impl Default for SharePanel {
    fn default() -> Self {
        let (picked_tx, picked_rx) = channel();
        Self { tab: Tab::Chat, input: String::new(), only_adif: false, browsed: false, picked_tx, picked_rx, picking: false, typing: false }
    }
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

fn is_adif_name(name: &str) -> bool {
    name.rsplit('.').next().is_some_and(|e| ADIF_EXTS.iter().any(|a| e.eq_ignore_ascii_case(a)))
}

impl SharePanel {
    /// Da chiamare a ogni frame con la sessione attiva: avvia gli invii dei file scelti o trascinati sulla finestra.
    pub fn poll(&mut self, ctx: &egui::Context, share: &Share, accept_drops: bool) {
        while let Ok(files) = self.picked_rx.try_recv() {
            self.picking = false;
            for f in files {
                share.upload(f);
            }
            self.tab = Tab::Files;
        }
        if accept_drops {
            let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
            if !dropped.is_empty() {
                self.tab = Tab::Files;
                for f in dropped {
                    share.upload(f);
                }
            }
        }
    }

    /// Apre la finestra di scelta file senza bloccare l'interfaccia.
    fn pick(&mut self, ctx: &egui::Context, title: &str, filter: Option<(&'static str, &'static [&'static str])>) {
        self.picking = true;
        let (tx, ctx, title) = (self.picked_tx.clone(), ctx.clone(), title.to_string());
        std::thread::spawn(move || {
            let mut dialog = rfd::FileDialog::new().set_title(title);
            if let Some((name, exts)) = filter {
                dialog = dialog.add_filter(name, exts);
            }
            let files = dialog.pick_files().unwrap_or_default();
            let _ = tx.send(files);
            ctx.request_repaint();
        });
    }

    /// Pannello completo del viewer: schede Chat e File, con sfoglia del PC remoto.
    pub fn viewer_ui(&mut self, ui: &mut egui::Ui, share: &Share) {
        ui.horizontal(|ui| {
            let unread = share.view.lock().unwrap().unread;
            let chat_label = if unread > 0 && self.tab != Tab::Chat { format!("Chat ({unread})") } else { "Chat".into() };
            ui.selectable_value(&mut self.tab, Tab::Chat, chat_label);
            ui.selectable_value(&mut self.tab, Tab::Files, "File");
        });
        ui.separator();
        match self.tab {
            Tab::Chat => {
                let height = ui.available_height() - 44.0;
                self.chat_ui(ui, share, "Host", height)
            }
            Tab::Files => self.files_ui(ui, share),
        }
    }

    /// Riquadro dell'host durante una sessione: chat e file ricevuti, affiancati.
    pub fn host_ui(&mut self, ui: &mut egui::Ui, share: &Share) {
        ui.columns(2, |cols| {
            cols[0].label(RichText::new("Chat con chi è collegato").strong());
            self.chat_ui(&mut cols[0], share, "Remoto", 150.0);
            cols[1].label(RichText::new("File scambiati").strong());
            cols[1].label(
                RichText::new(format!("Ricevuti in: {}", share.recv_dir.display())).color(Color32::GRAY).small(),
            );
            transfers_ui(&mut cols[1], share, None);
        });
    }

    fn chat_ui(&mut self, ui: &mut egui::Ui, share: &Share, peer: &str, height: f32) {
        share.mark_read();
        let lines = share.view.lock().unwrap().chat.clone();
        egui::ScrollArea::vertical()
            .id_salt("chat-scroll")
            .max_height(height.max(80.0))
            .min_scrolled_height(height.max(80.0))
            .auto_shrink(false)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if lines.is_empty() {
                    ui.label(RichText::new("Scrivi un messaggio: arriva subito all'altro lato.").color(Color32::GRAY));
                }
                for line in &lines {
                    let (who, color) = if line.mine { ("Tu", ACCENT) } else { (peer, WARN) };
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(format!("{who}:")).color(color).strong());
                        ui.label(&line.text);
                    });
                }
            });
        ui.horizontal(|ui| {
            let send_width = 64.0;
            let edit = ui.add(
                egui::TextEdit::singleline(&mut self.input)
                    .hint_text("Scrivi e premi Invio")
                    .desired_width(ui.available_width() - send_width - 12.0),
            );
            self.typing = edit.has_focus();
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let clicked = ui.add_enabled(!self.input.trim().is_empty(), egui::Button::new("Invia")).clicked();
            if (enter || clicked) && !self.input.trim().is_empty() {
                share.chat(&self.input);
                self.input.clear();
                edit.request_focus();
            }
        });
    }

    fn files_ui(&mut self, ui: &mut egui::Ui, share: &Share) {
        let ctx = ui.ctx().clone();
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(!self.picking, |ui| {
                if ui.button("Invia file…").clicked() {
                    self.pick(&ctx, "Scegli i file da inviare", None);
                }
                if ui.button("Invia immagine…").clicked() {
                    self.pick(&ctx, "Scegli le immagini da inviare", Some(("Immagini", IMAGE_EXTS)));
                }
                if ui.button("Invia log ADIF…").clicked() {
                    self.pick(&ctx, "Scegli i log ADIF da inviare", Some(("Log ADIF", ADIF_EXTS)));
                }
            });
        });
        ui.label(
            RichText::new("Puoi anche trascinare i file sulla finestra. Arrivano nella cartella di scambio dell'host.")
                .color(Color32::GRAY)
                .small(),
        );
        ui.separator();
        egui::ScrollArea::vertical().id_salt("files-scroll").auto_shrink(false).show(ui, |ui| {
            ui.label(RichText::new("Trasferimenti").strong());
            transfers_ui(ui, share, Some(&ctx));
            ui.add_space(6.0);
            ui.separator();
            self.browser_ui(ui, share);
        });
    }

    /// Esplora le cartelle del PC remoto e permette di scaricarne i file.
    fn browser_ui(&mut self, ui: &mut egui::Ui, share: &Share) {
        if !self.browsed {
            self.browsed = true;
            share.list("");
        }
        ui.label(RichText::new("PC remoto").strong());
        ui.horizontal_wrapped(|ui| {
            if ui.button("Inizio").clicked() {
                share.list("");
            }
            if ui.button("Cerca log ADIF").on_hover_text("Cerca i file .adi/.adif nelle cartelle di WSJT-X, JTDX, Decodium, Documenti…").clicked() {
                share.list(ADIF_PATH);
            }
            ui.checkbox(&mut self.only_adif, "Solo ADIF");
        });
        let (listing, busy) = {
            let view = share.view.lock().unwrap();
            (view.listing.clone(), view.listing_busy)
        };
        if busy {
            ui.label(RichText::new("Caricamento…").color(Color32::GRAY));
        }
        let Some(listing) = listing else { return };
        ui.horizontal_wrapped(|ui| {
            if let Some(parent) = &listing.parent {
                if ui.button("↑ Su").clicked() {
                    share.list(parent);
                }
            }
            let shown = match listing.path.as_str() {
                "" => "Punti di partenza".to_string(),
                ADIF_PATH => "Log ADIF trovati".to_string(),
                p => p.to_string(),
            };
            ui.label(RichText::new(shown).monospace().small());
        });
        if let Some(e) = &listing.error {
            ui.label(RichText::new(e).color(BAD));
        }
        let mut shown = 0;
        for entry in &listing.entries {
            if !entry.is_dir && self.only_adif && !is_adif_name(&entry.name) {
                continue;
            }
            shown += 1;
            ui.horizontal(|ui| {
                if entry.is_dir {
                    if ui.link(RichText::new(format!("{}/", entry.name)).strong()).clicked() {
                        share.list(&entry.path);
                    }
                } else {
                    if ui.small_button("Scarica").clicked() {
                        share.download(&entry.path, &entry.name);
                    }
                    let name = ui.label(&entry.name);
                    name.on_hover_text(&entry.path);
                    ui.label(RichText::new(human_size(entry.size)).color(Color32::GRAY).small());
                }
            });
        }
        if shown == 0 && !busy && listing.error.is_none() {
            ui.label(RichText::new("Nessun elemento.").color(Color32::GRAY));
        }
    }
}

/// Elenco dei trasferimenti con avanzamento. `ctx` abilita i pulsanti (annulla, mostra nella cartella).
fn transfers_ui(ui: &mut egui::Ui, share: &Share, ctx: Option<&egui::Context>) {
    let transfers: Vec<Transfer> = share.view.lock().unwrap().transfers.iter().rev().cloned().collect();
    if transfers.is_empty() {
        ui.label(RichText::new("Nessun file scambiato.").color(Color32::GRAY));
        return;
    }
    for t in transfers.iter().take(40) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(if t.upload { "↑" } else { "↓" }).strong());
            ui.label(&t.name);
            let (text, color) = match &t.status {
                Status::Waiting => ("in attesa".to_string(), WARN),
                Status::Running => (format!("{} / {}", human_size(t.done), human_size(t.size)), ACCENT),
                Status::Done => (format!("fatto · {}", human_size(t.size)), OK),
                Status::Failed(e) => (e.clone(), BAD),
            };
            ui.label(RichText::new(text).color(color).small());
            let active = matches!(t.status, Status::Waiting | Status::Running);
            if active && ctx.is_some() && ui.small_button("Annulla").clicked() {
                share.cancel(t.key);
            }
            if t.status == Status::Done && !t.upload {
                if let Some(path) = &t.path {
                    if ui.small_button("Mostra nella cartella").clicked() {
                        reveal(path);
                    }
                }
            }
        });
        if t.status == Status::Running && t.size > 0 {
            ui.add(egui::ProgressBar::new(t.done as f32 / t.size as f32).desired_height(6.0));
        }
    }
}

/// Apre Esplora risorse con il file selezionato.
fn reveal(path: &std::path::Path) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let arg = format!("/select,{}", path.display());
        let _ = std::process::Command::new("explorer").raw_arg(arg).creation_flags(0x0800_0000).spawn();
    }
    #[cfg(not(windows))]
    let _ = path;
}

/// Mostra un invito quando dei file vengono trascinati sulla finestra.
pub fn drop_hint(ctx: &egui::Context, area: egui::Rect) {
    if ctx.input(|i| i.raw.hovered_files.is_empty()) {
        return;
    }
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("drop-hint")));
    painter.rect_filled(area, 0.0, Color32::from_black_alpha(170));
    painter.text(
        area.center(),
        egui::Align2::CENTER_CENTER,
        "Rilascia i file per inviarli all'host",
        egui::FontId::proportional(22.0),
        Color32::WHITE,
    );
}
