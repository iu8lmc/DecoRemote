#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod autostart;
mod capture;
mod clip;
mod config;
mod cursor;
mod host;
mod input;
mod share;
mod share_ui;
mod viewer;

use anyhow::{anyhow, Result};

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_target(false).init();

    // Coordinate di cattura e di input devono essere in pixel fisici.
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let mut cfg = config::Config::load_or_create();
    let (mut connect_id, mut connect_pw) = (None, None);
    let mut minimized = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--server" => {
                cfg.server = args.next().ok_or_else(|| anyhow!("--server richiede un indirizzo"))?;
                cfg.save();
            }
            "--minimized" => minimized = true,
            "--connect" => connect_id = args.next(),
            "--password" => connect_pw = args.next(),
            "--help" | "-h" => {
                println!("uso: decoremote [--server HOST:PORTA] [--minimized] [--connect ID --password PASSWORD]");
                return Ok(());
            }
            other => return Err(anyhow!("argomento sconosciuto: {other}")),
        }
    }
    let auto_connect = connect_id.zip(connect_pw);

    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let handle = rt.handle().clone();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("DecoRemote")
            .with_inner_size([780.0, 460.0])
            .with_min_inner_size([620.0, 380.0]),
        ..Default::default()
    };
    eframe::run_native(
        "DecoRemote",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, handle, cfg, auto_connect, minimized)))),
    )
    .map_err(|e| anyhow!("errore interfaccia: {e}"))
}
