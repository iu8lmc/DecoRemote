//! Interfaccia grafica (egui): schermata principale e vista del desktop remoto.

use std::sync::{atomic::Ordering, Arc};

use deco_proto::msg::{Key, Mods, MouseBtn, ScreenInfo, ViewerMsg};
use eframe::egui::{self, Color32, RichText};
use tokio::runtime::Handle;

use crate::{
    config::{random_password, Config},
    host::{HostShared, HostStatus, APPROVAL_TIMEOUT},
    viewer::{Viewer, ViewerStatus},
};

const ACCENT: Color32 = Color32::from_rgb(0x2D, 0xB8, 0xA6);
const OK: Color32 = Color32::from_rgb(0x4C, 0xC3, 0x6B);
const WARN: Color32 = Color32::from_rgb(0xE5, 0xB5, 0x3C);
const BAD: Color32 = Color32::from_rgb(0xE5, 0x5B, 0x5B);

struct ViewerState {
    viewer: Viewer,
    texture: Option<egui::TextureHandle>,
    size: [usize; 2],
    last_mods: Mods,
    last_pos: Option<(f32, f32)>,
    /// Ultimo movimento del mouse locale sull'immagine: per un po' il cursore si disegna dove sta il
    /// mouse locale (senza attendere l'host), poi torna alla posizione riportata dall'host.
    last_move: std::time::Instant,
    cursor_tex: std::collections::HashMap<u64, egui::TextureHandle>,
    held: Vec<MouseBtn>,
    quality: u8,
    /// Statistiche mostrate nella barra, aggiornate una volta al secondo.
    stat_at: std::time::Instant,
    stat_frames: u64,
    stat_bytes: u64,
    stat_text: String,
}

pub struct App {
    rt: Handle,
    cfg: Config,
    host: Arc<HostShared>,
    peer_id: String,
    peer_pw: String,
    server_edit: String,
    notice: Option<String>,
    /// Esito dell'ultima verifica di rete (None = mai eseguita, Some("") = in corso).
    net_check: Arc<std::sync::Mutex<Option<String>>>,
    view: Option<ViewerState>,
    /// Eventi tastiera sottratti a egui mentre si controlla un host remoto.
    key_events: Vec<egui::Event>,
    /// Collegamento da avviare al primo frame (opzioni `--connect`/`--password`).
    auto_connect: Option<(String, String)>,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        rt: Handle,
        cfg: Config,
        auto_connect: Option<(String, String)>,
    ) -> Self {
        setup_style(&cc.egui_ctx);
        let host = HostShared::new(
            cfg.id.clone(),
            cfg.server.clone(),
            cfg.password.clone(),
            cfg.require_confirmation,
            cc.egui_ctx.clone(),
        );
        rt.spawn(crate::host::run(host.clone()));
        Self {
            rt,
            server_edit: cfg.server.clone(),
            cfg,
            host,
            peer_id: String::new(),
            peer_pw: String::new(),
            notice: None,
            net_check: Arc::new(std::sync::Mutex::new(None)),
            view: None,
            key_events: Vec::new(),
            auto_connect,
        }
    }

    fn connect(&mut self, ctx: &egui::Context) {
        let target: String = self.peer_id.chars().filter(|c| !c.is_whitespace()).collect();
        if target == self.cfg.id {
            self.notice = Some("Non puoi collegarti a questo stesso computer.".into());
            return;
        }
        self.notice = None;
        let viewer = Viewer::connect(&self.rt, ctx.clone(), self.cfg.server.clone(), target, self.peer_pw.clone());
        self.view = Some(ViewerState {
            viewer,
            texture: None,
            size: [1, 1],
            last_mods: Mods::default(),
            last_pos: None,
            last_move: std::time::Instant::now() - std::time::Duration::from_secs(10),
            cursor_tex: Default::default(),
            held: Vec::new(),
            quality: 60,
            stat_at: std::time::Instant::now(),
            stat_frames: 0,
            stat_bytes: 0,
            stat_text: String::new(),
        });
    }
}

fn setup_style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        style.visuals.selection.bg_fill = ACCENT;
        style.visuals.hyperlink_color = ACCENT;
        style.spacing.item_spacing = egui::vec2(10.0, 10.0);
        style.spacing.button_padding = egui::vec2(14.0, 7.0);
    });
}

fn format_id(id: &str) -> String {
    id.as_bytes()
        .chunks(3)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

impl eframe::App for App {
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        if self.view.is_none() {
            return;
        }
        // Durante il controllo remoto la tastiera appartiene all'host: egui non deve vederla,
        // altrimenti Invio/Spazio/Tab azionerebbero i pulsanti della barra.
        let (keys, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut raw.events).into_iter().partition(|e| {
            matches!(
                e,
                egui::Event::Key { .. }
                    | egui::Event::Text(_)
                    | egui::Event::Copy
                    | egui::Event::Cut
                    | egui::Event::Paste(_)
            )
        });
        raw.events = rest;
        self.key_events.extend(keys);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if let Some((id, pw)) = self.auto_connect.take() {
            (self.peer_id, self.peer_pw) = (id, pw);
            let ctx = ui.ctx().clone();
            self.connect(&ctx);
        }
        if self.view.is_some() {
            if viewer_ui(ui, self.view.as_mut().unwrap(), &mut self.key_events, &self.host) {
                self.view = None;
            }
        } else {
            self.key_events.clear();
            self.main_ui(ui);
        }
    }
}

impl App {
    fn main_ui(&mut self, ui: &mut egui::Ui) {
        let mut connect = false;
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading(RichText::new("DecoRemote").color(ACCENT).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (text, color) = match self.host.status() {
                        HostStatus::Online => ("Online".to_string(), OK),
                        HostStatus::Connecting => ("Connessione al server…".to_string(), WARN),
                        HostStatus::Offline(e) => (format!("Offline: {e}"), BAD),
                    };
                    ui.label(RichText::new(text).color(color));
                    ui.label(RichText::new("●").color(color));
                });
            });
            ui.separator();
            approval_prompt(ui, &self.host);

            ui.columns(2, |cols| {
                self.this_computer_ui(&mut cols[0]);
                connect = self.remote_ui(&mut cols[1]);
            });

            ui.add_space(8.0);
            egui::CollapsingHeader::new("Impostazioni").show(ui, |ui| {
                if ui
                    .checkbox(&mut self.cfg.require_confirmation, "Chiedi conferma a ogni nuova connessione")
                    .on_hover_text("Se disattivata, chi conosce ID e password entra senza che tu debba consentire.")
                    .changed()
                {
                    self.cfg.save();
                    self.host.require_confirmation.store(self.cfg.require_confirmation, Ordering::SeqCst);
                }
                ui.horizontal(|ui| {
                    let running = self.net_check.lock().unwrap().as_deref() == Some("");
                    if ui.add_enabled(!running, egui::Button::new("Verifica rete")).clicked() {
                        self.start_net_check(ui.ctx().clone());
                    }
                    match self.net_check.lock().unwrap().as_deref() {
                        Some("") => {
                            ui.label("Verifica in corso…");
                        }
                        Some(text) => {
                            ui.label(text);
                        }
                        None => {
                            ui.label(
                                RichText::new("Controlla se da questa rete è possibile il collegamento diretto (UDP).")
                                    .color(Color32::GRAY),
                            );
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Server (indirizzo:porta)");
                    ui.text_edit_singleline(&mut self.server_edit);
                    if ui.button("Applica").clicked() {
                        let server = self.server_edit.trim().to_string();
                        if !server.is_empty() {
                            self.cfg.server = server.clone();
                            self.cfg.save();
                            *self.host.server.lock().unwrap() = server;
                            self.host.reconnect.notify_one();
                        }
                    }
                });
            });
        });
        if connect {
            let ctx = ui.ctx().clone();
            self.connect(&ctx);
        }
    }

    /// Verifica UDP e tipo di NAT verso il server configurato, senza bloccare la UI.
    fn start_net_check(&self, ctx: egui::Context) {
        *self.net_check.lock().unwrap() = Some(String::new());
        let (result, server) = (self.net_check.clone(), self.cfg.server.clone());
        self.rt.spawn(async move {
            let report = tokio::task::spawn_blocking(move || -> String {
                let addr = match deco_proto::udp::resolve_v4(&server) {
                    Ok(a) => a,
                    Err(e) => return format!("Server non risolvibile: {e}"),
                };
                let sock = match std::net::UdpSocket::bind("0.0.0.0:0") {
                    Ok(s) => s,
                    Err(e) => return format!("Impossibile aprire un socket UDP: {e}"),
                };
                match deco_proto::udp::probe_nat(&sock, addr) {
                    Err(_) => "UDP non raggiunge il server (bloccato o server senza UDP): si userà il relay TCP.".into(),
                    Ok(r) => match r.punchable() {
                        Some(true) => {
                            format!("OK: indirizzo pubblico {}. Il collegamento diretto è possibile da questa rete.", r.observed)
                        }
                        Some(false) => format!(
                            "NAT simmetrico ({} / {}): il collegamento diretto non è possibile, si userà il relay.",
                            r.observed,
                            r.second.map(|s| s.to_string()).unwrap_or_default()
                        ),
                        None => format!(
                            "Indirizzo pubblico {}. Il server non offre la seconda porta UDP: tipo di NAT non verificabile.",
                            r.observed
                        ),
                    },
                }
            })
            .await
            .unwrap_or_else(|e| format!("Verifica fallita: {e}"));
            *result.lock().unwrap() = Some(report);
            ctx.request_repaint();
        });
    }

    fn this_computer_ui(&mut self, ui: &mut egui::Ui) {
        ui.group(|ui| {
            ui.set_min_height(210.0);
            ui.label(RichText::new("Questo computer").strong().size(16.0));
            ui.label("Comunica ID e password per ricevere assistenza.");
            ui.add_space(4.0);

            ui.label("Il tuo ID");
            ui.horizontal(|ui| {
                ui.label(RichText::new(format_id(&self.cfg.id)).monospace().size(26.0).color(ACCENT));
                if ui.button("Copia").clicked() {
                    ui.ctx().copy_text(self.cfg.id.clone());
                }
            });

            ui.label("Password");
            ui.horizontal(|ui| {
                ui.label(RichText::new(&self.cfg.password).monospace().size(18.0));
                if ui.button("Copia").clicked() {
                    ui.ctx().copy_text(self.cfg.password.clone());
                }
                if ui.button("Rigenera").clicked() {
                    self.cfg.password = random_password();
                    self.cfg.save();
                    *self.host.password.lock().unwrap() = self.cfg.password.clone();
                }
            });

            if self.host.active.load(Ordering::SeqCst) {
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(RichText::new("● Sessione remota in corso").color(WARN));
                    if ui.button("Termina").clicked() {
                        self.host.stop_session.notify_waiters();
                    }
                });
            }
        });
    }

    /// Restituisce `true` se l'utente ha chiesto di connettersi.
    fn remote_ui(&mut self, ui: &mut egui::Ui) -> bool {
        let mut connect = false;
        ui.group(|ui| {
            ui.set_min_height(210.0);
            ui.label(RichText::new("Controlla un computer").strong().size(16.0));
            ui.label("Inserisci ID e password del computer da controllare.");
            ui.add_space(4.0);

            ui.label("ID partner");
            let id_resp = ui.add(egui::TextEdit::singleline(&mut self.peer_id).desired_width(f32::INFINITY));
            ui.label("Password");
            let pw_resp =
                ui.add(egui::TextEdit::singleline(&mut self.peer_pw).password(true).desired_width(f32::INFINITY));

            let ready = !self.peer_id.trim().is_empty() && !self.peer_pw.is_empty();
            let enter = (id_resp.lost_focus() || pw_resp.lost_focus())
                && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.add_enabled(ready, egui::Button::new("Connetti").fill(ACCENT)).clicked() || (ready && enter) {
                connect = true;
            }
            if let Some(n) = &self.notice {
                ui.label(RichText::new(n).color(BAD));
            }
        });
        connect
    }
}

/// Richiesta di connessione in arrivo su questo computer: Consenti / Rifiuta, con conto alla rovescia.
fn approval_prompt(ui: &mut egui::Ui, host: &HostShared) {
    let Some(since) = host.pending_since() else { return };
    let left = APPROVAL_TIMEOUT.saturating_sub(since.elapsed()).as_secs() + 1;
    egui::Frame::new()
        .fill(Color32::from_rgb(0x3A, 0x2E, 0x10))
        .stroke(egui::Stroke::new(1.5, WARN))
        .corner_radius(6.0)
        .inner_margin(10.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Qualcuno vuole controllare questo computer.").color(WARN).strong().size(15.0),
                );
                ui.label(format!("Rifiuto automatico tra {left} s"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(egui::Button::new("Rifiuta").fill(Color32::from_rgb(0x8A, 0x2F, 0x2F))).clicked() {
                        host.answer(false);
                    }
                    if ui.add(egui::Button::new("Consenti").fill(ACCENT)).clicked() {
                        host.answer(true);
                    }
                });
            });
        });
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
}

/// Disegna la vista remota. Restituisce `true` quando la sessione va chiusa.
fn viewer_ui(
    root: &mut egui::Ui,
    vs: &mut ViewerState,
    key_events: &mut Vec<egui::Event>,
    host: &HostShared,
) -> bool {
    let ctx = root.ctx().clone();
    let mut close = false;
    let status = vs.viewer.status();

    egui::Panel::top("bar").show(root, |ui| {
        approval_prompt(ui, host);
        ui.horizontal(|ui| {
            match &status {
                ViewerStatus::Connecting => ui.label(RichText::new("Connessione…").color(WARN)),
                ViewerStatus::WaitingApproval => {
                    ui.label(RichText::new("In attesa che l'host accetti…").color(WARN))
                }
                ViewerStatus::Connected { host_name, .. } => ui.label(RichText::new(format!("● {host_name}")).color(OK)),
                ViewerStatus::Closed(_) => ui.label(RichText::new("Disconnesso").color(BAD)),
            };
            if let ViewerStatus::Connected { screens, current, .. } = &status {
                if screens.len() > 1 {
                    let label = |i: usize, s: &ScreenInfo| {
                        let main = if s.primary { " · principale" } else { "" };
                        format!("Schermo {} ({}×{}){main}", i + 1, s.width, s.height)
                    };
                    let mut selected = *current;
                    egui::ComboBox::from_id_salt("screen")
                        .selected_text(screens.get(*current).map(|s| label(*current, s)).unwrap_or_default())
                        .show_ui(ui, |ui| {
                            for (i, s) in screens.iter().enumerate() {
                                ui.selectable_value(&mut selected, i, label(i, s));
                            }
                        });
                    if selected != *current {
                        vs.viewer.send(ViewerMsg::SelectScreen(selected as u32));
                        vs.last_pos = None;
                    }
                }
                ui.label(RichText::new(&vs.stat_text).color(Color32::GRAY).small());
                ui.label("Qualità");
                if ui.add(egui::Slider::new(&mut vs.quality, 20..=95).show_value(false)).changed() {
                    vs.viewer.send(ViewerMsg::SetQuality(vs.quality));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Disconnetti").clicked() {
                    close = true;
                }
            });
        });
    });

    let update = vs.viewer.frame.lock().unwrap().take();
    if let Some(up) = update {
        let opts = egui::TextureOptions::LINEAR;
        if vs.texture.is_none() || vs.size != up.size {
            // Prima immagine o cambio di risoluzione: texture nuova (nera dove mancano dati).
            vs.size = up.size;
            let base = up.full.unwrap_or_else(|| egui::ColorImage::filled(up.size, Color32::BLACK));
            vs.texture = Some(ctx.load_texture("remote-screen", base, opts));
        } else if let (Some(tex), Some(full)) = (&mut vs.texture, up.full) {
            tex.set(full, opts);
        }
        if let Some(tex) = &mut vs.texture {
            for (y, img) in up.parts {
                tex.set_partial([0, y], img, opts);
            }
        }
    }
    if vs.stat_at.elapsed() >= std::time::Duration::from_secs(1) {
        let (frames, bytes) = (vs.viewer.stats.frames.load(Ordering::Relaxed), vs.viewer.stats.bytes.load(Ordering::Relaxed));
        let dt = vs.stat_at.elapsed().as_secs_f64();
        let codec = if vs.viewer.stats.av1.load(Ordering::Relaxed) { "AV1" } else { "JPEG" };
        let path = if vs.viewer.stats.direct.load(Ordering::Relaxed) {
            format!("QUIC diretto {} ms", vs.viewer.stats.rtt_ms.load(Ordering::Relaxed))
        } else {
            "relay TCP".to_string()
        };
        vs.stat_text = format!(
            "{codec} · {:.0} fps · {:.1} Mbit/s · {path}",
            (frames - vs.stat_frames) as f64 / dt,
            (bytes - vs.stat_bytes) as f64 * 8.0 / dt / 1e6
        );
        (vs.stat_at, vs.stat_frames, vs.stat_bytes) = (std::time::Instant::now(), frames, bytes);
    }
    ctx.request_repaint_after(std::time::Duration::from_secs(1));

    egui::CentralPanel::default().frame(egui::Frame::new().fill(Color32::BLACK)).show(root, |ui| {
        if let ViewerStatus::Closed(reason) = &status {
            ui.centered_and_justified(|ui| {
                let text = reason.clone().unwrap_or_else(|| "La sessione è terminata.".into());
                ui.label(RichText::new(text).color(BAD).size(18.0));
            });
            return;
        }
        let Some(tex) = &vs.texture else {
            ui.centered_and_justified(|ui| {
                let text = match status {
                    ViewerStatus::WaitingApproval => "In attesa che l'utente remoto consenta l'accesso…",
                    _ => "In attesa dello schermo remoto…",
                };
                ui.label(RichText::new(text).color(Color32::GRAY).size(16.0));
            });
            return;
        };
        let avail = ui.available_rect_before_wrap();
        let [w, h] = vs.size;
        let scale = (avail.width() / w as f32).min(avail.height() / h as f32);
        let rect = egui::Rect::from_center_size(avail.center(), egui::vec2(w as f32, h as f32) * scale);
        ui.painter().image(
            tex.id(),
            rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        forward_input(&ctx, vs, rect, std::mem::take(key_events));
        draw_cursor(&ctx, ui, vs, rect);
    });

    close
}

/// Disegna il cursore dell'host sopra l'immagine. Mentre l'utente muove il mouse lo si mette dove
/// sta il mouse locale: l'host seguirà entro pochi millisecondi, ma il cursore non aspetta la rete.
fn draw_cursor(ctx: &egui::Context, ui: &egui::Ui, vs: &mut ViewerState, rect: egui::Rect) {
    const LOCAL_PRIORITY: std::time::Duration = std::time::Duration::from_millis(400);
    let (host_pos, shape_id, host_visible, shape) = {
        let cs = vs.viewer.cursor.lock().unwrap();
        (cs.pos, cs.shape, cs.visible, cs.shapes.get(&cs.shape).cloned())
    };
    let Some(shape) = shape else { return }; // forma non ancora arrivata: resta il cursore di sistema
    let hover = ctx.input(|i| i.pointer.hover_pos()).filter(|p| rect.contains(*p));
    if hover.is_some() {
        ctx.set_cursor_icon(egui::CursorIcon::None);
    }
    let pos = match hover {
        Some(p) if vs.last_move.elapsed() < LOCAL_PRIORITY => p,
        _ if host_visible => rect.min + egui::vec2(host_pos.0 * rect.width(), host_pos.1 * rect.height()),
        Some(p) => p,
        None => return,
    };
    if !host_visible && hover.is_none() {
        return;
    }
    if !vs.cursor_tex.contains_key(&shape_id) {
        if vs.cursor_tex.len() > 64 {
            vs.cursor_tex.clear();
        }
        let tex = ctx.load_texture(format!("cursor-{shape_id}"), shape.image.clone(), egui::TextureOptions::LINEAR);
        vs.cursor_tex.insert(shape_id, tex);
    }
    let scale = rect.width() / vs.size[0].max(1) as f32;
    let size = egui::vec2(shape.image.size[0] as f32, shape.image.size[1] as f32) * scale;
    let top_left = pos - egui::vec2(shape.hot_x as f32, shape.hot_y as f32) * scale;
    if let Some(tex) = vs.cursor_tex.get(&shape_id) {
        ui.painter().image(
            tex.id(),
            egui::Rect::from_min_size(top_left, size),
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }
}

fn sync_mods(vs: &mut ViewerState, m: egui::Modifiers) {
    // Su Windows AltGr arriva come Ctrl+Alt: lo si lascia gestire ai caratteri (Text).
    let altgr = m.ctrl && m.alt;
    let mods = Mods { ctrl: m.ctrl && !altgr, alt: m.alt && !altgr, shift: m.shift, meta: m.mac_cmd };
    if mods != vs.last_mods {
        vs.last_mods = mods;
        vs.viewer.send(ViewerMsg::Mods(mods));
    }
}

fn forward_input(ctx: &egui::Context, vs: &mut ViewerState, rect: egui::Rect, key_events: Vec<egui::Event>) {
    let (hover, events, current_mods) = ctx.input(|i| (i.pointer.hover_pos(), i.events.clone(), i.modifiers));
    let norm = |p: egui::Pos2| {
        (
            ((p.x - rect.min.x) / rect.width()).clamp(0.0, 1.0),
            ((p.y - rect.min.y) / rect.height()).clamp(0.0, 1.0),
        )
    };

    sync_mods(vs, current_mods);

    if let Some(p) = hover.filter(|p| rect.contains(*p)) {
        let n = norm(p);
        if vs.last_pos != Some(n) {
            vs.last_pos = Some(n);
            vs.last_move = std::time::Instant::now();
            vs.viewer.send(ViewerMsg::MouseMove { x: n.0, y: n.1 });
        }
    }

    for ev in events {
        match ev {
            egui::Event::PointerButton { pos, button, pressed, .. } => {
                let b = match button {
                    egui::PointerButton::Primary => MouseBtn::Left,
                    egui::PointerButton::Secondary => MouseBtn::Right,
                    egui::PointerButton::Middle => MouseBtn::Middle,
                    _ => continue,
                };
                let (x, y) = norm(pos);
                if pressed && rect.contains(pos) {
                    vs.held.push(b);
                    vs.viewer.send(ViewerMsg::MouseButton { button: b, pressed, x, y });
                } else if !pressed && vs.held.contains(&b) {
                    // Rilascio inoltrato anche fuori dall'immagine, per non lasciare il pulsante premuto.
                    vs.held.retain(|&h| h != b);
                    vs.viewer.send(ViewerMsg::MouseButton { button: b, pressed, x, y });
                }
            }
            egui::Event::MouseWheel { unit, delta, .. } if hover.is_some_and(|p| rect.contains(p)) => {
                let k = match unit {
                    egui::MouseWheelUnit::Point => 1.0,
                    egui::MouseWheelUnit::Line => 40.0,
                    egui::MouseWheelUnit::Page => 400.0,
                };
                vs.viewer.send(ViewerMsg::Wheel { dx: delta.x * k, dy: delta.y * k });
            }
            _ => {}
        }
    }

    for ev in key_events {
        match ev {
            egui::Event::Text(t) => vs.viewer.send(ViewerMsg::Text(t)),
            egui::Event::Key { key, pressed, modifiers, .. } => {
                sync_mods(vs, modifiers);
                if let Some(k) = map_key(key, &modifiers) {
                    vs.viewer.send(ViewerMsg::Key { key: k, pressed });
                }
            }
            // egui trasforma Ctrl+C/X/V in questi eventi invece che in Key.
            egui::Event::Copy => tap(vs, 'c'),
            egui::Event::Cut => tap(vs, 'x'),
            egui::Event::Paste(text) => {
                // Gli appunti locali arrivano all'host prima del Ctrl+V.
                vs.viewer.send(ViewerMsg::Clipboard(text));
                tap(vs, 'v');
            }
            _ => {}
        }
    }
}

fn tap(vs: &mut ViewerState, c: char) {
    sync_mods(vs, egui::Modifiers::CTRL);
    vs.viewer.send(ViewerMsg::Key { key: Key::Char(c), pressed: true });
    vs.viewer.send(ViewerMsg::Key { key: Key::Char(c), pressed: false });
}

fn map_key(key: egui::Key, mods: &egui::Modifiers) -> Option<Key> {
    use egui::Key as K;
    let shortcut = (mods.ctrl != mods.alt) || mods.mac_cmd;
    Some(match key {
        K::Enter => Key::Enter,
        K::Backspace => Key::Backspace,
        K::Tab => Key::Tab,
        K::Escape => Key::Escape,
        K::Delete => Key::Delete,
        K::Insert => Key::Insert,
        K::Home => Key::Home,
        K::End => Key::End,
        K::PageUp => Key::PageUp,
        K::PageDown => Key::PageDown,
        K::ArrowLeft => Key::Left,
        K::ArrowRight => Key::Right,
        K::ArrowUp => Key::Up,
        K::ArrowDown => Key::Down,
        K::F1 => Key::F(1),
        K::F2 => Key::F(2),
        K::F3 => Key::F(3),
        K::F4 => Key::F(4),
        K::F5 => Key::F(5),
        K::F6 => Key::F(6),
        K::F7 => Key::F(7),
        K::F8 => Key::F(8),
        K::F9 => Key::F(9),
        K::F10 => Key::F(10),
        K::F11 => Key::F(11),
        K::F12 => Key::F(12),
        K::Space if shortcut => Key::Space,
        // I caratteri normali arrivano già come Text: qui servono solo per le scorciatoie (Ctrl+A, Alt+F...).
        other if shortcut => {
            let name = other.name();
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Key::Char(c.to_ascii_lowercase()),
                _ => return None,
            }
        }
        _ => return None,
    })
}
