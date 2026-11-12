//! Запуск приложения: один экземпляр, потоки, трей на главном потоке.
//!
//! Потоки: главный — трей (цикл сообщений Win32); хук клавиатуры; контроллер; микрофон;
//! сеть (tokio на одном потоке); лог. В простое все ждут на своих очередях — 0 % CPU.

use std::time::Instant;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, LPARAM, WPARAM};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW};

use crate::audio::recorder::{self, AudioCmd};
use crate::config::{self, Config, Paths};
use crate::controller::{Channels, Controller, Parts};
use crate::hotkey::hook::{HookThread, MATCHER};
use crate::net::service::NetService;
use crate::net::Route;
use crate::tray::{self, UiState};
use crate::{error, info, logger, warn};

/// Второй экземпляр не запускается: он просит первый показать уведомление и выходит.
fn single_instance() -> bool {
    let name: Vec<u16> = "Local\\Verba.SingleInstance".encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        // хэндл не закрываем: мьютекс живёт, пока жив процесс
        let _mutex = CreateMutexW(None, false, PCWSTR(name.as_ptr()));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(hwnd) = FindWindowW(tray::WINDOW_CLASS, PCWSTR::null()) {
                let _ = PostMessageW(Some(hwnd), tray::WM_SECOND_INSTANCE, WPARAM(0), LPARAM(0));
            }
            return false;
        }
    }
    true
}

pub fn run() -> i32 {
    if !single_instance() {
        return 0;
    }
    let epoch = Instant::now();
    unsafe {
        // значок трея рисуется под реальный DPI, без размытия масштабированием
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let paths = Paths::from_env();
    logger::init(paths.log_file());
    std::panic::set_hook(Box::new(|p| {
        let thread = std::thread::current().name().unwrap_or("?").to_string();
        error!("сбой в потоке {thread}: {p}");
        logger::flush();
    }));
    info!("Verba {} запущена", env!("CARGO_PKG_VERSION"));

    let mut startup_error = None;
    let (cfg, bindings, cfg_modified) = match config::load(&paths.config_file()) {
        Ok(l) => (l.config, l.bindings, l.modified),
        Err(e) => {
            warn!("{e}");
            startup_error = Some(e);
            let mut c = Config::default();
            let b = c.validate().expect("настройки по умолчанию верны");
            (c, b, config::modified(&paths.config_file()))
        }
    };
    MATCHER.set_bindings(&bindings);

    let (ui_tx, ui_rx) = crossbeam_channel::unbounded();
    let ui = match tray::create(UiState::default(), ui_tx) {
        Ok(u) => u,
        Err(e) => {
            error!("{e}");
            logger::flush();
            return 1;
        }
    };
    if let Some(e) = startup_error {
        ui.notify(e);
    }

    let (hook_tx, hook_rx) = crossbeam_channel::unbounded();
    let hook = match HookThread::spawn(hook_tx) {
        Ok(h) => h,
        Err(e) => {
            error!("{e}");
            ui.notify(format!("{e} → перезапусти Verba"));
            tray::run_loop();
            return 1;
        }
    };
    let (audio_ev_tx, audio_rx) = crossbeam_channel::unbounded();
    let audio = match recorder::spawn(audio_ev_tx) {
        Ok(a) => a,
        Err(e) => {
            error!("поток микрофона: {e}");
            return 1;
        }
    };
    let _ = audio.send(AudioCmd::Configure(cfg.microphone.clone()));
    let (net_ev_tx, net_rx) = crossbeam_channel::unbounded();
    // маршрут без прокси — контроллер сразу пришлёт настоящий (с паролем из Credential Manager)
    let net = match NetService::spawn(Route { base_url: cfg.openrouter.base_url.clone(), proxy: None }, net_ev_tx) {
        Ok(n) => n,
        Err(e) => {
            error!("сетевой поток: {e}");
            return 1;
        }
    };
    let (paste_tx, paste_rx) = crossbeam_channel::unbounded();
    info!(
        "хоткеи: диктовка {} ({:?}), пауза {}, вставить последнее {}, отмена {}",
        bindings.dictate.label(),
        cfg.hotkeys.mode,
        bindings.pause.label(),
        bindings.paste_last.label(),
        bindings.cancel.label()
    );

    let parts = Parts { paths, cfg, bindings, cfg_modified, ui, net, audio, hook, paste_tx, epoch };
    let channels = Channels { hook: hook_rx, audio: audio_rx, net: net_rx, ui: ui_rx, paste: paste_rx };
    let spawned = std::thread::Builder::new().name("verba-control".into()).spawn(move || {
        Controller::new(parts).run(channels);
    });
    if let Err(e) = spawned {
        error!("поток контроллера: {e}");
        return 1;
    }
    tray::run_loop();
    info!("Verba завершена");
    logger::flush();
    0
}
