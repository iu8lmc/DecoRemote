//! Cursore del mouse dell'host: forma e posizione, inviati a parte rispetto al video
//! (la cattura DXGI non include il puntatore) così il viewer lo disegna da sé, senza ritardo.

use std::{
    sync::{atomic::AtomicBool, Arc},
    thread,
};

use deco_proto::msg::HostMsg;
use tokio::sync::mpsc::UnboundedSender;

use crate::capture::Screens;

pub fn spawn(tx: UnboundedSender<HostMsg>, screens: Arc<Screens>, stop: Arc<AtomicBool>) {
    #[cfg(windows)]
    thread::spawn(move || win::run(tx, &screens, &stop));
    #[cfg(not(windows))]
    {
        let _ = (tx, screens, stop, thread::current());
    }
}

#[cfg(windows)]
mod win {
    use std::{
        collections::HashMap,
        ffi::c_void,
        mem::{size_of, zeroed},
        ptr,
        sync::atomic::Ordering,
        time::Duration,
    };

    use deco_proto::msg::HostMsg;
    use tokio::sync::mpsc::UnboundedSender;
    use windows_sys::Win32::{
        Graphics::Gdi::{
            CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetObjectW, ReleaseDC, SelectObject,
            BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        },
        UI::WindowsAndMessaging::{
            DrawIconEx, GetCursorInfo, GetIconInfo, CURSORINFO, CURSOR_SHOWING, DI_NORMAL, ICONINFO,
        },
    };

    use crate::capture::Screens;

    /// Oltre questa dimensione il cursore (anche personalizzato) non viene inviato.
    const MAX_SIDE: i32 = 128;
    const MAX_CACHED_SHAPES: usize = 64;
    const POLL: Duration = Duration::from_millis(8);

    struct Shape {
        width: u32,
        height: u32,
        hot_x: i32,
        hot_y: i32,
        rgba: Vec<u8>,
    }

    pub fn run(tx: UnboundedSender<HostMsg>, screens: &Screens, stop: &std::sync::atomic::AtomicBool) {
        // handle del cursore di sistema -> id della forma già inviata al viewer
        let mut sent: HashMap<usize, u64> = HashMap::new();
        let mut next_id = 1u64;
        let mut last: Option<(u64, bool, i32, i32)> = None;

        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(POLL);
            let rect = screens.rect();
            if rect.w == 0 || rect.h == 0 {
                continue; // cattura non ancora avviata
            }
            // SAFETY: strutture C inizializzate con cbSize prima della chiamata.
            let info = unsafe {
                let mut info: CURSORINFO = zeroed();
                info.cbSize = size_of::<CURSORINFO>() as u32;
                if GetCursorInfo(&mut info) == 0 {
                    continue;
                }
                info
            };

            let handle = info.hCursor as usize;
            let mut shape_id = 0u64;
            if handle != 0 {
                if let Some(&id) = sent.get(&handle) {
                    shape_id = id;
                } else if let Some(shape) = unsafe { render(info.hCursor) } {
                    if sent.len() >= MAX_CACHED_SHAPES {
                        sent.clear();
                    }
                    shape_id = next_id;
                    next_id += 1;
                    sent.insert(handle, shape_id);
                    let msg = HostMsg::CursorShape {
                        id: shape_id,
                        width: shape.width,
                        height: shape.height,
                        hot_x: shape.hot_x,
                        hot_y: shape.hot_y,
                        rgba: shape.rgba,
                    };
                    if tx.send(msg).is_err() {
                        return;
                    }
                }
            }

            let (px, py) = (info.ptScreenPos.x - rect.x, info.ptScreenPos.y - rect.y);
            let inside = px >= 0 && py >= 0 && (px as u32) < rect.w && (py as u32) < rect.h;
            let visible = shape_id != 0 && inside && info.flags & CURSOR_SHOWING != 0;
            let state = (shape_id, visible, px, py);
            if last != Some(state) {
                last = Some(state);
                let msg = HostMsg::Cursor {
                    x: (px as f32 / rect.w as f32).clamp(0.0, 1.0),
                    y: (py as f32 / rect.h as f32).clamp(0.0, 1.0),
                    shape: shape_id,
                    visible,
                };
                if tx.send(msg).is_err() {
                    return;
                }
            }
        }
    }

    /// Disegna il cursore due volte, su fondo nero e su fondo bianco: dalla differenza si ricava
    /// l'alfa anche per i cursori monocromatici e a inversione, che non hanno un canale alfa proprio.
    ///
    /// # Safety
    /// `cursor` deve essere un handle di cursore valido.
    #[allow(unsafe_op_in_unsafe_fn)] // intero corpo unsafe: il contratto è nel # Safety
    unsafe fn render(cursor: *mut c_void) -> Option<Shape> {
        let mut icon: ICONINFO = zeroed();
        if GetIconInfo(cursor, &mut icon) == 0 {
            return None;
        }
        let result = render_with(cursor, &icon);
        if !icon.hbmMask.is_null() {
            DeleteObject(icon.hbmMask);
        }
        if !icon.hbmColor.is_null() {
            DeleteObject(icon.hbmColor);
        }
        result
    }

    #[allow(unsafe_op_in_unsafe_fn)]
    unsafe fn render_with(cursor: *mut c_void, icon: &ICONINFO) -> Option<Shape> {
        // Dimensioni: dalla bitmap a colori; nei cursori monocromatici la maschera è alta il doppio.
        let mut bm: BITMAP = zeroed();
        let (src, halve) = if icon.hbmColor.is_null() { (icon.hbmMask, true) } else { (icon.hbmColor, false) };
        if GetObjectW(src, size_of::<BITMAP>() as i32, &mut bm as *mut _ as *mut c_void) == 0 {
            return None;
        }
        let (w, h) = (bm.bmWidth, if halve { bm.bmHeight / 2 } else { bm.bmHeight });
        if w <= 0 || h <= 0 || w > MAX_SIDE || h > MAX_SIDE {
            return None;
        }

        let screen = GetDC(ptr::null_mut());
        let dc = CreateCompatibleDC(screen);
        let mut header: BITMAPINFO = zeroed();
        header.bmiHeader = BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // dall'alto in basso
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..zeroed()
        };
        let mut bits: *mut c_void = ptr::null_mut();
        let dib = CreateDIBSection(dc, &header, DIB_RGB_COLORS, &mut bits, ptr::null_mut(), 0);
        let mut out = None;
        if !dib.is_null() && !bits.is_null() {
            let old = SelectObject(dc, dib);
            let len = (w * h * 4) as usize;
            let buf = std::slice::from_raw_parts_mut(bits as *mut u8, len);

            buf.fill(0x00);
            DrawIconEx(dc, 0, 0, cursor, w, h, 0, ptr::null_mut(), DI_NORMAL);
            let on_black = buf.to_vec();
            buf.fill(0xFF);
            DrawIconEx(dc, 0, 0, cursor, w, h, 0, ptr::null_mut(), DI_NORMAL);
            let on_white = buf.to_vec();

            let mut rgba = vec![0u8; len];
            for i in (0..len).step_by(4) {
                // Per canale: nero = c*a, bianco = c*a + 255*(1-a)  =>  a = 255 - (bianco - nero).
                let diff: i32 = (0..3).map(|c| on_white[i + c] as i32 - on_black[i + c] as i32).sum::<i32>() / 3;
                let alpha = (255 - diff).clamp(0, 255);
                if alpha == 0 {
                    continue;
                }
                // Il DIB è BGRA: si riordina in RGBA e si toglie la premoltiplicazione.
                for (dst, src) in [(0, 2), (1, 1), (2, 0)] {
                    rgba[i + dst] = ((on_black[i + src] as i32 * 255) / alpha).clamp(0, 255) as u8;
                }
                rgba[i + 3] = alpha as u8;
            }
            out = Some(Shape {
                width: w as u32,
                height: h as u32,
                hot_x: icon.xHotspot as i32,
                hot_y: icon.yHotspot as i32,
                rgba,
            });
            SelectObject(dc, old);
        }
        if !dib.is_null() {
            DeleteObject(dib);
        }
        DeleteDC(dc);
        ReleaseDC(ptr::null_mut(), screen);
        out
    }
}
