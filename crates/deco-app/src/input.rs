//! Iniezione di mouse e tastiera sul computer controllato.

use std::{
    sync::{mpsc::Receiver, Arc},
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use deco_proto::msg::{Key, Mods, MouseBtn, ViewerMsg};
use enigo::{Axis, Button, Direction, Enigo, Key as EKey, Keyboard, Mouse, Settings};
use tracing::warn;

use crate::capture::Screens;

/// Pixel di scroll del viewer che corrispondono a una "tacca" della rotella.
const PIXELS_PER_NOTCH: f32 = 40.0;

/// Intervallo minimo fra due segnalazioni di errore al viewer.
const REPORT_EVERY: Duration = Duration::from_secs(30);

/// `report` riceve il testo da mostrare al viewer quando l'host non riesce a iniettare l'input
/// (di solito perché Windows lo blocca: finestra avviata come amministratore, `BlockInput`, desktop protetto).
pub fn spawn(rx: Receiver<ViewerMsg>, screens: Arc<Screens>, report: impl Fn(String) + Send + 'static) {
    thread::spawn(move || {
        if let Err(e) = run(rx, &screens, &report) {
            warn!("input: {e:#}");
            report(format!("Attenzione: l'host non può iniettare mouse e tastiera ({e:#})."));
        }
    });
}

#[derive(Default)]
struct State {
    mods: Mods,
    buttons: Vec<MouseBtn>,
    scroll_x: f32,
    scroll_y: f32,
}

fn run(rx: Receiver<ViewerMsg>, screens: &Screens, report: &impl Fn(String)) -> Result<()> {
    let mut enigo = Enigo::new(&Settings::default())?;
    let mut st = State::default();
    let mut last_report: Option<Instant> = None;
    while let Ok(msg) = rx.recv() {
        if let Err(e) = handle(&mut enigo, &mut st, screens, msg) {
            warn!("input rifiutato: {e:#}");
            if last_report.is_none_or(|t| t.elapsed() >= REPORT_EVERY) {
                last_report = Some(Instant::now());
                report(format!(
                    "Attenzione: Windows rifiuta l'input su questo computer ({e:#}). {}",
                    elevation_hint()
                ));
            }
        }
    }
    // Sessione finita: rilascia tutto per non lasciare tasti o pulsanti "incastrati".
    for b in std::mem::take(&mut st.buttons) {
        let _ = enigo.button(button(b), Direction::Release);
    }
    let _ = set_mods(&mut enigo, &mut st, Mods::default());
    Ok(())
}

fn handle(enigo: &mut Enigo, st: &mut State, screens: &Screens, msg: ViewerMsg) -> Result<()> {
    match msg {
        ViewerMsg::MouseMove { x, y } => move_to(enigo, screens, x, y)?,
        ViewerMsg::MouseButton { button: b, pressed, x, y } => {
            move_to(enigo, screens, x, y)?;
            if pressed {
                if !st.buttons.contains(&b) {
                    st.buttons.push(b);
                }
                enigo.button(button(b), Direction::Press)?;
            } else {
                st.buttons.retain(|&held| held != b);
                enigo.button(button(b), Direction::Release)?;
            }
        }
        ViewerMsg::Wheel { dx, dy } => {
            // egui: delta positivo = scorrere verso l'alto; enigo: positivo = verso il basso.
            st.scroll_y -= dy / PIXELS_PER_NOTCH;
            st.scroll_x -= dx / PIXELS_PER_NOTCH;
            let (ny, nx) = (st.scroll_y.trunc(), st.scroll_x.trunc());
            st.scroll_y -= ny;
            st.scroll_x -= nx;
            if ny != 0.0 {
                enigo.scroll(ny as i32, Axis::Vertical)?;
            }
            if nx != 0.0 {
                enigo.scroll(nx as i32, Axis::Horizontal)?;
            }
        }
        ViewerMsg::Mods(m) => set_mods(enigo, st, m)?,
        ViewerMsg::Key { key, pressed } => {
            if let Some(k) = map_key(key) {
                let dir = if pressed { Direction::Press } else { Direction::Release };
                enigo.key(k, dir)?;
            }
        }
        ViewerMsg::Text(text) => enigo.text(&text)?,
        ViewerMsg::Clipboard(_) | ViewerMsg::SetQuality(_) | ViewerMsg::SelectScreen(_)
        | ViewerMsg::SetCodec(_)
        | ViewerMsg::RequestKeyframe
        | ViewerMsg::Share(_)
        | ViewerMsg::Bye => {}
    }
    Ok(())
}

fn move_to(enigo: &mut Enigo, screens: &Screens, x: f32, y: f32) -> Result<()> {
    let r = screens.rect();
    if r.w == 0 || r.h == 0 {
        return Ok(()); // cattura non ancora avviata
    }
    // Coordinate nel desktop virtuale: l'origine dello schermo può essere negativa.
    let px = r.x + (x.clamp(0.0, 1.0) * (r.w - 1) as f32).round() as i32;
    let py = r.y + (y.clamp(0.0, 1.0) * (r.h - 1) as f32).round() as i32;
    #[cfg(windows)]
    {
        // enigo in modalità assoluta indirizza solo il monitor principale.
        let _ = enigo;
        // SAFETY: chiamata Win32 senza puntatori.
        if unsafe { windows_sys::Win32::UI::WindowsAndMessaging::SetCursorPos(px, py) } == 0 {
            anyhow::bail!("SetCursorPos fallita");
        }
    }
    #[cfg(not(windows))]
    enigo.move_mouse(px, py, enigo::Coordinate::Abs)?;
    Ok(())
}

fn set_mods(enigo: &mut Enigo, st: &mut State, new: Mods) -> Result<()> {
    let old = st.mods;
    let changes = [
        (old.ctrl, new.ctrl, EKey::Control),
        (old.alt, new.alt, EKey::Alt),
        (old.shift, new.shift, EKey::Shift),
        (old.meta, new.meta, EKey::Meta),
    ];
    for (was, now, key) in changes {
        if was != now {
            enigo.key(key, if now { Direction::Press } else { Direction::Release })?;
        }
    }
    st.mods = new;
    Ok(())
}

fn button(b: MouseBtn) -> Button {
    match b {
        MouseBtn::Left => Button::Left,
        MouseBtn::Right => Button::Right,
        MouseBtn::Middle => Button::Middle,
    }
}

fn map_key(key: Key) -> Option<EKey> {
    Some(match key {
        Key::Enter => EKey::Return,
        Key::Backspace => EKey::Backspace,
        Key::Tab => EKey::Tab,
        Key::Escape => EKey::Escape,
        Key::Delete => EKey::Delete,
        Key::Home => EKey::Home,
        Key::End => EKey::End,
        Key::PageUp => EKey::PageUp,
        Key::PageDown => EKey::PageDown,
        Key::Left => EKey::LeftArrow,
        Key::Right => EKey::RightArrow,
        Key::Up => EKey::UpArrow,
        Key::Down => EKey::DownArrow,
        Key::Space => EKey::Space,
        Key::Insert => EKey::Other(0x2D), // VK_INSERT
        Key::F(n) => match n {
            1 => EKey::F1,
            2 => EKey::F2,
            3 => EKey::F3,
            4 => EKey::F4,
            5 => EKey::F5,
            6 => EKey::F6,
            7 => EKey::F7,
            8 => EKey::F8,
            9 => EKey::F9,
            10 => EKey::F10,
            11 => EKey::F11,
            12 => EKey::F12,
            _ => return None,
        },
        // Lettere e cifre arrivano qui solo con Ctrl/Alt premuti (scorciatoie): servono i
        // virtual-key code, perché l'input Unicode non attiva le scorciatoie.
        Key::Char(c) if c.is_ascii_alphanumeric() => EKey::Other(c.to_ascii_uppercase() as u32),
        Key::Char(c) => EKey::Unicode(c),
    })
}

/// Suggerimento sulla causa più comune: l'host non ha i privilegi della finestra che riceve l'input.
fn elevation_hint() -> &'static str {
    if is_elevated() {
        "DecoRemote è avviato come amministratore: controlla se un altro programma blocca l'input (BlockInput) o se è aperta una schermata protetta (UAC, blocco)."
    } else {
        "DecoRemote non è avviato come amministratore: Windows non lascia inviare click e tasti ai programmi che lo sono. Riavvia DecoRemote con «Esegui come amministratore»."
    }
}

#[cfg(windows)]
pub fn is_elevated() -> bool {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY},
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };
    // SAFETY: handle e buffer locali, `GetTokenInformation` scrive al più `size_of::<TOKEN_ELEVATION>()` byte.
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut _,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

#[cfg(not(windows))]
pub fn is_elevated() -> bool {
    false
}
