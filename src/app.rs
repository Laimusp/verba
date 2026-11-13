//! Запуск приложения: один экземпляр, потоки, трей и окно настроек на главном потоке.
//!
//! Потоки: главный — цикл Slint (он же раздаёт сообщения окну трея); хук клавиатуры; контроллер;
//! микрофон; сеть (tokio на одном потоке); оверлей; лог. В простое все ждут на своих очередях — 0 % CPU.
//!
//! Запуск руками (`verba.exe`) открывает окно настроек; из автозапуска (`verba.exe --tray`) — только
//! значок в трее. Второй запуск не стартует второй экземпляр, а просит первый показать окно. Если
//! включён автозапуск с правами, а Verba запустили без прав, — она запускает себя через задачу
//! планировщика (с правами, без окна UAC) и сама выходит.

use std::path::Path;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, LPARAM, WPARAM};
use windows::Win32::System::Threading::{CreateMutexW, OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, FindWindowW, PostMessageW, ASFW_ANY};

use crate::audio::recorder::{self, AudioCmd};
use crate::autostart;
use crate::config::{self, Config, Paths};
use crate::controller::{Channels, Controller, Parts};
use crate::hotkey::hook::{self, HookThread, MATCHER};
use crate::net::service::NetService;
use crate::net::Route;
use crate::overlay::Overlay;
use crate::settings;
use crate::target::TargetService;
use crate::tray::{self, UiState};
use crate::{error, info, logger, warn};

const MUTEX: &str = "Local\\Verba.SingleInstance";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Другой экземпляр уже работает. Мьютекс экземпляра с правами процессу без прав не открыть —
/// отказ в доступе значит то же: экземпляр есть.
fn other_running(name: &[u16]) -> bool {
    unsafe {
        match OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr())) {
            Ok(h) => {
                let _ = CloseHandle(h);
                true
            }
            Err(e) => e.code() == ERROR_ACCESS_DENIED.to_hresult(),
        }
    }
}

/// Занять мьютекс экземпляра; хэндл не закрываем — мьютекс живёт, пока жив процесс.
fn take_mutex(name: &[u16]) -> bool {
    unsafe {
        match CreateMutexW(None, false, PCWSTR(name.as_ptr())) {
            Ok(h) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    let _ = CloseHandle(h);
                    false
                } else {
                    true
                }
            }
            // мьютекс создан процессом с правами — нам его не открыть: экземпляр уже есть
            Err(_) => false,
        }
    }
}

/// Попросить работающий экземпляр показать окно настроек. Его окно трея ждём до `wait`: экземпляр,
/// запущенный через задачу планировщика, ещё стартует. Окну разрешаем встать вперёд: этот процесс
/// запущен человеком и сейчас впереди, тот — в фоне (foreground lock).
fn ask_to_show(wait: Duration) -> bool {
    let until = Instant::now() + wait;
    loop {
        if let Ok(hwnd) = unsafe { FindWindowW(tray::WINDOW_CLASS, PCWSTR::null()) } {
            unsafe {
                let _ = AllowSetForegroundWindow(ASFW_ANY);
                let _ = PostMessageW(Some(hwnd), tray::WM_SECOND_INSTANCE, WPARAM(0), LPARAM(0));
            }
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Задача автозапуска с правами есть, включена и запускает этот exe.
fn task_runs_this_exe() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    match autostart::task_state() {
        Ok(autostart::TaskState::Present { exe: task_exe, enabled: true }) => same_path(&task_exe, &exe),
        _ => false,
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().trim_matches('"').replace('/', "\\").to_lowercase();
    norm(a) == norm(b)
}

/// Метка «показать окно после перезапуска» (кнопка «Перезапустить» в окне): свежая — показать.
fn take_show_flag(paths: &Paths) -> bool {
    let flag = crate::controller::show_flag(paths);
    let fresh = std::fs::metadata(&flag)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < Duration::from_secs(120));
    let _ = std::fs::remove_file(&flag);
    fresh
}

/// `tray_only` — запуск из автозапуска: без окна настроек.
pub fn run(tray_only: bool) -> i32 {
    let name = wide(MUTEX);
    if !tray_only {
        if other_running(&name) {
            ask_to_show(Duration::from_secs(3));
            return 0;
        }
        // без прав, а автозапуск с правами включён — тот же exe с правами через задачу, без окна UAC
        if !autostart::is_elevated() && task_runs_this_exe() && autostart::run_task().is_ok() && ask_to_show(Duration::from_secs(10)) {
            return 0;
        }
    }
    // из задачи после «Перезапустить»: прежний экземпляр ещё закрывается — подождать его
    let wait = if tray_only { Duration::from_secs(8) } else { Duration::ZERO };
    let started = Instant::now();
    while !take_mutex(&name) {
        if started.elapsed() >= wait {
            if !tray_only {
                ask_to_show(Duration::from_secs(3));
            }
            return 0;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let epoch = Instant::now();
    unsafe {
        // значок трея и оверлей рисуются под реальный DPI, без размытия масштабированием
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let paths = Paths::from_env();
    logger::init(paths.log_file());
    std::panic::set_hook(Box::new(|p| {
        let thread = std::thread::current().name().unwrap_or("?").to_string();
        error!("сбой в потоке {thread}: {p}");
        logger::flush();
    }));
    let elevated = autostart::is_elevated();
    info!(
        "Verba {} запущена{}{}",
        env!("CARGO_PKG_VERSION"),
        if elevated { " с правами администратора" } else { "" },
        if tray_only { " (автозапуск)" } else { "" }
    );
    let show_window = !tray_only || take_show_flag(&paths);

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
    settings::init(ui_tx.clone());
    let ui = match tray::create(UiState::default(), ui_tx.clone()) {
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
    let (capture_tx, capture_rx) = crossbeam_channel::unbounded();
    hook::set_capture_sender(capture_tx);
    let hook = match HookThread::spawn(hook_tx) {
        Ok(h) => h,
        Err(e) => {
            error!("{e}");
            ui.notify(format!("{e} → перезапусти Verba"));
            let _ = slint::run_event_loop_until_quit();
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
    let (target_tx, target_rx) = crossbeam_channel::unbounded();
    let targets = match TargetService::spawn(target_tx) {
        Ok(t) => t,
        Err(e) => {
            error!("{e}");
            return 1;
        }
    };
    let overlay = match Overlay::spawn() {
        Ok(o) => Some(o),
        Err(e) => {
            // без плашки диктовка работает: ошибки уйдут уведомлениями трея
            warn!("оверлей записи не запустился: {e}");
            None
        }
    };
    info!(
        "хоткеи: диктовка {} ({:?}), пауза {}, вставить последнее {}, отмена {}",
        bindings.dictate.label(),
        cfg.hotkeys.mode,
        bindings.pause.label(),
        bindings.paste_last.label(),
        bindings.cancel.label()
    );

    let parts = Parts { paths, cfg, bindings, cfg_modified, ui, net, audio, hook, paste_tx, targets, overlay, ui_tx, epoch };
    let channels =
        Channels { hook: hook_rx, audio: audio_rx, net: net_rx, ui: ui_rx, paste: paste_rx, target: target_rx, capture: capture_rx };
    let spawned = std::thread::Builder::new().name("verba-control".into()).spawn(move || {
        Controller::new(parts).run(channels);
    });
    if let Err(e) = spawned {
        error!("поток контроллера: {e}");
        return 1;
    }
    // цикл Slint (winit) при старте забирает Raw Input клавиатуры себе — вернуть его сторожу хука
    slint::Timer::single_shot(Duration::ZERO, move || hook.raw_input(true));
    if show_window {
        settings::open_at_start();
    }
    if let Err(e) = slint::run_event_loop_until_quit() {
        error!("цикл окна: {e}");
    }
    info!("Verba завершена");
    logger::flush();
    0
}
