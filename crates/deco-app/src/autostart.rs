//! Avvio automatico con Windows (chiave `Run` dell'utente corrente, senza privilegi di amministratore).

#[cfg(windows)]
mod imp {
    use std::{os::windows::process::CommandExt, process::Command};

    const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const NAME: &str = "DecoRemote";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    fn reg(args: &[&str]) -> Option<std::process::Output> {
        Command::new("reg").args(args).creation_flags(CREATE_NO_WINDOW).output().ok()
    }

    pub fn is_enabled() -> bool {
        reg(&["query", KEY, "/v", NAME]).is_some_and(|o| o.status.success())
    }

    pub fn set(enabled: bool) -> bool {
        let out = if enabled {
            let Ok(exe) = std::env::current_exe() else { return false };
            let command = format!("\"{}\" --minimized", exe.display());
            reg(&["add", KEY, "/v", NAME, "/t", "REG_SZ", "/d", &command, "/f"])
        } else {
            reg(&["delete", KEY, "/v", NAME, "/f"])
        };
        out.is_some_and(|o| o.status.success())
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn is_enabled() -> bool {
        false
    }

    pub fn set(_enabled: bool) -> bool {
        false
    }
}

pub use imp::{is_enabled, set};
