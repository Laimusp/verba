//! Окно настроек со стороны контроллера: действия окна → настройки (сразу в дело и в config.json),
//! состояние окна → `ViewData`. Долгое (сеть, список микрофонов, планировщик задач, окно UAC) идёт
//! в сетевом потоке или в короткоживущем рабочем потоке; итог возвращается событием `Worker`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::Controller;
use crate::autostart;
use crate::config::{Engine, HotkeyMode, ProxyConfig, ProxyKind};
use crate::hotkey::binding::{Binding, MOD_ALT, MOD_CTRL, MOD_SHIFT, MOD_WIN};
use crate::hotkey::hook::{self, HotkeyId, MATCHER};
use crate::net::codex::{self, Login};
use crate::net::proxy::{self, ParsedProxy};
use crate::net::service::NetCmd;
use crate::net::Model;
use crate::secrets;
use crate::settings::{self, Cmd, Flag, HotkeyItem, Line, ModelItem, Msg, ViewData, HOTKEY_ROWS};
use crate::tray::UiEvent;
use crate::{info, warn};

/// Захват сочетания ждёт нажатия не дольше этого.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(15);

/// Итог фоновой работы для контроллера.
#[derive(Debug, Clone)]
pub enum Worker {
    /// имена микрофонов Windows
    Devices(Result<Vec<String>, String>),
    /// есть ли задача автозапуска с правами на этот exe
    AutostartState(Result<bool, String>),
    AutostartDone { on: bool, result: Result<(), String> },
    Shortcut(Result<PathBuf, String>),
    /// запуск Verba с правами через задачу (перезапуск)
    Restarted(Result<(), String>),
}

#[derive(Default)]
pub struct Prefs {
    pub open: bool,
    /// «sk-or-v1-…abcd»; None — ключа нет
    pub key_mask: Option<String>,
    key_line: Line,
    /// ключ из окна ждёт проверки OpenRouter, сохранится после неё
    key_candidate: Option<String>,
    /// проверка из окна (а не из трея): итог — строкой в окне
    key_check_from_window: bool,
    catalog: Option<Vec<Model>>,
    catalog_line: Line,
    proxy_line: Line,
    /// прокси из окна ждёт проверки или «сохранить всё равно»
    proxy_pending: Option<ParsedProxy>,
    proxy_unsaved: bool,
    codex_login: Line,
    codex_line: Line,
    capture: Option<(HotkeyId, Instant)>,
    hotkey_warn: [String; 4],
    devices: Option<Vec<String>>,
    autostart: bool,
    autostart_busy: bool,
    autostart_line: Line,
    elevated: Option<bool>,
}

fn row_index(id: HotkeyId) -> usize {
    HOTKEY_ROWS.iter().position(|r| *r == id).unwrap_or(0)
}

fn hotkey_title(id: HotkeyId, mode: HotkeyMode) -> (&'static str, &'static str) {
    match id {
        HotkeyId::Dictate => (
            "Говорить",
            match mode {
                HotkeyMode::Hold => "Держи и говори",
                HotkeyMode::Toggle => "Нажми, говори, нажми ещё раз",
                HotkeyMode::Hybrid => "Держи или нажми",
            },
        ),
        HotkeyId::Pause => ("Пауза", "Хоткей уходит в приложения нетронутым"),
        HotkeyId::PasteLast => ("Вставить последнее", "Не распозналось — отправит запись снова"),
        HotkeyId::Cancel => ("Отмена", "Только во время записи"),
    }
}

/// Сочетания, которые обычно чем-то заняты в Windows и программах: Verba их перехватит раньше.
pub fn common_conflict(b: Binding) -> Option<&'static str> {
    let key = b.to_config();
    let key = key.rsplit('+').next().unwrap_or("");
    let m = b.mods;
    let word = match (m, key) {
        (MOD_CTRL, "c") => "копирование",
        (MOD_CTRL, "v") => "вставка",
        (MOD_CTRL, "x") => "вырезание",
        (MOD_CTRL, "z") => "отмена действия",
        (MOD_CTRL, "a") => "выделить всё",
        (MOD_CTRL, "s") => "сохранение",
        (MOD_CTRL, "f") => "поиск",
        (MOD_CTRL, "w") => "закрыть вкладку",
        (MOD_CTRL, "t") => "новая вкладка",
        (MOD_CTRL, "tab") => "переключение вкладок",
        (MOD_ALT, "tab") => "переключение окон",
        (MOD_ALT, "f4") => "закрыть окно",
        (MOD_ALT, "space") => "меню окна",
        (MOD_CTRL, "esc") => "меню «Пуск»",
        (x, "esc") if x == MOD_CTRL | MOD_SHIFT => "Диспетчер задач",
        (MOD_WIN, "l") => "блокировка экрана — Windows его не отдаёт",
        (MOD_WIN, "h") => "голосовой ввод Windows",
        (MOD_WIN, "v") => "журнал буфера обмена",
        (MOD_WIN, "d" | "e" | "r" | "tab" | "period" | "i" | "a" | "s" | "x") => "сочетание Windows",
        _ => return None,
    };
    Some(word)
}

/// Предупреждение к сочетанию строки `id`: занято системой, другой программой, печатает текст.
fn warn_for(id: HotkeyId, b: Binding) -> String {
    if id == HotkeyId::Cancel {
        return String::new();
    }
    if b.mods == 0 || b.mods == MOD_SHIFT {
        let printable = matches!(b.vk, 0x20 | 0x30..=0x39 | 0x41..=0x5A | 0xBA..=0xC0 | 0xDB..=0xDE | 0x0D | 0x09 | 0x08);
        if printable {
            return "Без Ctrl или Alt клавиша перестанет печататься, пока Verba слушает".into();
        }
    }
    if let Some(w) = common_conflict(b) {
        return format!("Обычно это {w}: Verba заберёт сочетание у программ");
    }
    if taken_by_other_program(b) {
        return "Занято другой программой: Verba перехватит его раньше неё".into();
    }
    String::new()
}

/// Сочетание уже зарегистрировано глобально другой программой (`RegisterHotKey` отказывает).
fn taken_by_other_program(b: Binding) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT as W_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT as W_SHIFT,
        MOD_WIN as W_WIN,
    };
    let mut mods = MOD_NOREPEAT;
    for (bit, w) in [(MOD_CTRL, MOD_CONTROL), (MOD_ALT, W_ALT), (MOD_SHIFT, W_SHIFT), (MOD_WIN, W_WIN)] {
        if b.mods & bit != 0 {
            mods |= w;
        }
    }
    const PROBE_ID: i32 = 0x5642;
    unsafe {
        if RegisterHotKey(None, PROBE_ID, HOT_KEY_MODIFIERS(mods.0), u32::from(b.vk)).is_ok() {
            let _ = UnregisterHotKey(None, PROBE_ID);
            false
        } else {
            true
        }
    }
}

fn kind_words(k: ProxyKind) -> &'static str {
    match k {
        ProxyKind::Auto => "Прокси",
        ProxyKind::Http => "HTTP",
        ProxyKind::Https => "HTTPS",
        ProxyKind::Socks4 => "SOCKS4",
        ProxyKind::Socks5 => "SOCKS5",
    }
}

fn proxy_words(kind: ProxyKind, host: &str, port: u16, with_login: bool) -> String {
    format!("{} {host}:{port}{}", kind_words(kind), if with_login { " · с логином" } else { "" })
}

fn sec(ms: u64) -> String {
    super::sec(ms)
}

fn spawn_worker(tx: crossbeam_channel::Sender<UiEvent>, name: &str, f: impl FnOnce() -> Worker + Send + 'static) {
    let _ = std::thread::Builder::new().name(format!("verba-{name}")).spawn(move || {
        let _ = tx.send(UiEvent::Worker(f()));
    });
}

/// Микрофоны Windows по именам (cpal, WASAPI). В рабочем потоке: перечисление бывает долгим.
fn list_devices() -> Result<Vec<String>, String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let devices = host.input_devices().map_err(|e| format!("список микрофонов не получен: {e}"))?;
    let mut names: Vec<String> = devices.filter_map(|d| d.name().ok()).collect();
    names.sort();
    names.dedup();
    Ok(names)
}

/// Открыть адрес в браузере по умолчанию. В своём потоке: ShellExecute бывает медленным.
fn open_url(url: String) {
    let _ = std::thread::Builder::new().name("verba-open".into()).spawn(move || {
        use windows::core::{w, PCWSTR};
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let u: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            ShellExecuteW(None, w!("open"), PCWSTR(u.as_ptr()), None, None, SW_SHOWNORMAL);
        }
    });
}

/// Файл-метка «после перезапуска показать окно настроек»: новый экземпляр с правами стартует из
/// задачи планировщика в трей, а человек нажал «Перезапустить» в окне — окно должно вернуться.
pub fn show_flag(paths: &crate::config::Paths) -> PathBuf {
    paths.data_dir.join("show-settings.flag")
}

impl Controller {
    pub(super) fn on_settings(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Opened => {
                // пока окно настроек открыто, Raw Input у процесса снят: иначе хук молчит (hook.rs)
                self.hook.raw_input(false);
                self.prefs.open = true;
                *self.shown_view.borrow_mut() = None;
                self.refresh_codex_login();
                self.refresh_hotkey_warnings();
                if self.prefs.catalog.is_none() && !self.prefs.catalog_line.is_busy() {
                    self.ask_models();
                }
                let tx = self.ui_tx.clone();
                spawn_worker(tx, "mics", || Worker::Devices(list_devices()));
                if self.prefs.elevated.is_none() {
                    self.prefs.elevated = Some(autostart::is_elevated());
                }
                if !self.prefs.autostart_busy {
                    let tx = self.ui_tx.clone();
                    spawn_worker(tx, "autostart", || Worker::AutostartState(autostart_state()));
                }
            }
            Cmd::Closed => {
                self.hook.raw_input(true);
                self.prefs.open = false;
                self.stop_capture();
            }
            Cmd::Engine(e) => {
                self.select_engine(e);
                if e == Engine::Codex {
                    self.refresh_codex_login();
                }
            }
            Cmd::KeySave(k) => self.key_save(k),
            Cmd::KeyCheck => match secrets::read(secrets::OPENROUTER_KEY) {
                Some(key) => {
                    self.prefs.key_candidate = None;
                    self.prefs.key_check_from_window = true;
                    self.prefs.key_line = Line::busy("Спрашиваю OpenRouter…");
                    self.net.send(NetCmd::CheckKey { key });
                }
                None => {
                    self.has_key = false;
                    self.prefs.key_mask = None;
                    self.prefs.key_line = Line::err("Ключа нет → вставь его и нажми «Сохранить»");
                }
            },
            Cmd::KeyRemove => {
                secrets::delete(secrets::OPENROUTER_KEY);
                info!("ключ OpenRouter удалён из Credential Manager");
                self.has_key = false;
                self.prefs.key_mask = None;
                self.prefs.key_line = Line::default();
            }
            Cmd::ModelsRefresh => self.ask_models(),
            Cmd::ModelActive(id) => {
                if id != self.cfg.openrouter.model {
                    info!("модель OpenRouter: {id}");
                    self.set(|c| c.openrouter.model = id);
                }
            }
            Cmd::ModelAdd(id) => {
                let id = id.trim().to_string();
                if !id.is_empty() && !id.contains(char::is_whitespace) {
                    self.set(|c| {
                        if !c.openrouter.models.contains(&id) {
                            c.openrouter.models.push(id);
                        }
                    });
                    settings::push(Msg::ModelAdded);
                }
            }
            Cmd::ModelRemove(id) => {
                if id != self.cfg.openrouter.model {
                    self.set(|c| c.openrouter.models.retain(|m| *m != id));
                }
            }
            Cmd::ProxyCheck(text) => self.proxy_check(text),
            Cmd::ProxySaveAnyway => {
                if let Some(p) = self.prefs.proxy_pending.take() {
                    let kind = p.kind.unwrap_or(ProxyKind::Auto);
                    let (host, port, login) = (p.host.clone(), p.port, p.user.is_some());
                    if self.save_proxy(p, kind) {
                        self.prefs.proxy_line = Line::ok(format!("{} — сохранён без проверки", proxy_words(kind, &host, port, login)));
                        settings::push(Msg::ProxySaved);
                    }
                }
                self.prefs.proxy_unsaved = false;
            }
            Cmd::ProxyRemove => {
                secrets::delete(secrets::PROXY_PASSWORD);
                self.prefs.proxy_pending = None;
                self.prefs.proxy_unsaved = false;
                self.prefs.proxy_line = Line::default();
                info!("прокси убран");
                self.set(|c| c.proxy = None);
            }
            Cmd::CodexCheck => {
                self.refresh_codex_login();
                info!("проверка движка ChatGPT: 0,3 с тишины");
                self.prefs.codex_line = Line::busy("Шлю 0,3 с тишины в ChatGPT…");
                self.net.send(NetCmd::CheckCodex);
            }
            Cmd::Capture(id) => self.start_capture(id),
            Cmd::CaptureCancel => self.stop_capture(),
            Cmd::Mode(m) => self.set(|c| c.hotkeys.mode = m),
            Cmd::HoldMs(ms) => self.set(|c| c.hotkeys.hybrid_hold_ms = ms),
            Cmd::Flag(f, on) => self.set(|c| match f {
                Flag::RememberField => c.paste.remember_field = on,
                Flag::TrailingSpace => c.paste.trailing_space = on,
                Flag::Humanize => c.paste.humanize = on,
                Flag::RestoreClipboard => c.paste.restore_clipboard = on,
                Flag::KeepOpen => c.microphone.keep_open = on,
                Flag::SoundRecording => c.sounds.recording = on,
                Flag::SoundPause => c.sounds.pause = on,
                Flag::SoundErrors => c.sounds.errors = on,
            }),
            Cmd::GameAdd(g) => {
                self.set(|c| c.paste.games.push(g));
                settings::push(Msg::GameAdded);
            }
            Cmd::GameRemove(g) => self.set(|c| c.paste.games.retain(|x| *x != g)),
            Cmd::Device(i) => {
                let device = if i == 0 { None } else { self.device_list().get(i).cloned() };
                if i == 0 || device.is_some() {
                    info!("микрофон: {}", device.as_deref().unwrap_or("как в Windows"));
                    self.set(|c| c.microphone.device = device);
                }
            }
            Cmd::Overlay(p) => self.set(|c| c.overlay = p),
            Cmd::Autostart(on) => self.autostart(on),
            Cmd::Restart => self.restart_elevated(),
            Cmd::OpenLog => super::open_file(&self.paths.log_file()),
            Cmd::OpenConfig => super::open_file(&self.paths.config_file()),
            Cmd::OpenUrl(u) => open_url(u),
            Cmd::Shortcut => {
                let tx = self.ui_tx.clone();
                spawn_worker(tx, "shortcut", || {
                    Worker::Shortcut(
                        std::env::current_exe()
                            .map_err(|e| format!("путь к Verba не узнать: {e}"))
                            .and_then(|exe| autostart::create_desktop_shortcut(&exe)),
                    )
                });
            }
            Cmd::TogglePause => self.toggle_pause(),
            Cmd::Exit => {}
        }
        self.publish();
    }

    pub(super) fn on_worker(&mut self, w: Worker) {
        match w {
            Worker::Devices(r) => match r {
                Ok(d) => self.prefs.devices = Some(d),
                Err(e) => {
                    warn!("{e}");
                    self.prefs.devices = Some(Vec::new());
                }
            },
            Worker::AutostartState(r) => match r {
                Ok(on) => {
                    self.prefs.autostart = on;
                    if self.prefs.autostart_line.is_busy() {
                        self.prefs.autostart_line = Line::default();
                    }
                }
                Err(e) => {
                    warn!("автозапуск: {e}");
                    self.prefs.autostart_line = Line::err(e);
                }
            },
            Worker::AutostartDone { on, result } => {
                self.prefs.autostart_busy = false;
                match result {
                    Ok(()) => {
                        info!("автозапуск с правами {}", if on { "включён" } else { "выключен" });
                        self.prefs.autostart = on;
                        self.prefs.autostart_line = if on {
                            Line::ok("После входа в Windows Verba запустится с правами")
                        } else {
                            Line::ok("Выключен")
                        };
                    }
                    Err(e) => {
                        warn!("автозапуск: {e}");
                        self.prefs.autostart_line = Line::err(e);
                    }
                }
            }
            Worker::Shortcut(r) => match r {
                Ok(p) => {
                    info!("ярлык Verba на рабочем столе: {}", p.display());
                    self.prefs.autostart_line = Line::ok("Ярлык Verba — на рабочем столе");
                }
                Err(e) => {
                    warn!("ярлык: {e}");
                    self.prefs.autostart_line = Line::err(e);
                }
            },
            Worker::Restarted(r) => match r {
                Ok(()) => {
                    info!("перезапуск с правами: задача запущена — закрываюсь");
                    self.shutdown();
                }
                Err(e) => {
                    let _ = std::fs::remove_file(show_flag(&self.paths));
                    warn!("перезапуск с правами: {e}");
                    self.prefs.autostart_line = Line::err(e);
                }
            },
        }
        self.publish();
    }

    // ---------- ключ ----------

    fn key_save(&mut self, key: String) {
        let key = key.trim().to_string();
        if key.len() < 20 || key.contains(char::is_whitespace) {
            self.prefs.key_line = Line::err("Это не ключ OpenRouter → скопируй ключ вида sk-or-v1-… целиком");
            return;
        }
        self.prefs.key_line = Line::busy("Проверяю ключ у OpenRouter…");
        self.prefs.key_candidate = Some(key.clone());
        self.prefs.key_check_from_window = true;
        self.net.send(NetCmd::CheckKey { key });
    }

    fn store_key(&mut self, key: &str) -> bool {
        if let Err(e) = secrets::write(secrets::OPENROUTER_KEY, "openrouter", key) {
            warn!("{e}");
            self.prefs.key_line = Line::err(e);
            return false;
        }
        info!("ключ OpenRouter сохранён в Credential Manager");
        self.has_key = true;
        self.error = None;
        self.prefs.key_mask = Some(settings::mask_key(key));
        settings::push(Msg::KeySaved);
        true
    }

    pub(super) fn on_key_checked(&mut self, r: Result<(u16, u64), String>) {
        let from_window = std::mem::take(&mut self.prefs.key_check_from_window);
        let candidate = self.prefs.key_candidate.take();
        let line = match (candidate, r) {
            (Some(k), Ok((200, ms))) => {
                if !self.store_key(&k) {
                    self.publish();
                    return;
                }
                Line::ok(format!("Ключ принят и сохранён, ответ за {}", sec(ms)))
            }
            (Some(_), Ok((401 | 403, _))) => Line::err("OpenRouter не принял ключ → скопируй его заново целиком"),
            (Some(k), Ok((s, _))) => {
                if !self.store_key(&k) {
                    self.publish();
                    return;
                }
                Line::err(format!("Ключ сохранён; OpenRouter ответил {s} — проверь диктовкой"))
            }
            (Some(k), Err(e)) => {
                if !self.store_key(&k) {
                    self.publish();
                    return;
                }
                Line::err(format!("Ключ сохранён, но проверить не удалось: {e}"))
            }
            (None, Ok((200, ms))) => Line::ok(format!("Ключ работает, ответ за {}", sec(ms))),
            (None, Ok((401 | 403, _))) => Line::err("OpenRouter не принял ключ → замени его"),
            (None, Ok((s, _))) => Line::err(format!("OpenRouter ответил {s}")),
            (None, Err(e)) => Line::err(format!("Проверить не удалось: {e}")),
        };
        info!("проверка ключа: {}", line.text);
        if !from_window || !self.prefs.open {
            self.ui.notify(line.text.clone());
        }
        self.prefs.key_line = line;
        self.publish();
    }

    // ---------- модели ----------

    fn ask_models(&mut self) {
        self.prefs.catalog_line = Line::busy("Спрашиваю OpenRouter…");
        self.net.send(NetCmd::ListModels);
    }

    pub(super) fn on_models(&mut self, r: Result<Vec<Model>, String>) {
        match r {
            Ok(m) => {
                info!("OpenRouter: моделей распознавания {}", m.len());
                self.prefs.catalog = Some(m);
                self.prefs.catalog_line = Line::default();
            }
            Err(e) => {
                warn!("список моделей OpenRouter: {e}");
                self.prefs.catalog_line = Line::err(format!("Список моделей не пришёл: {e}"));
            }
        }
        self.publish();
    }

    // ---------- прокси ----------

    fn proxy_check(&mut self, text: String) {
        let (p, saved) = if text.trim().is_empty() {
            let Some(c) = self.cfg.proxy.clone() else { return };
            let kind = match c.kind {
                ProxyKind::Auto => self.detected_proxy.get(&(c.host.clone(), c.port)).copied(),
                k => Some(k),
            };
            let pass = secrets::read(secrets::PROXY_PASSWORD);
            (ParsedProxy { kind, host: c.host, port: c.port, user: c.user, pass }, true)
        } else {
            match proxy::parse(&text) {
                Ok(p) => (p, false),
                Err(e) => {
                    self.prefs.proxy_line = Line::err(e);
                    self.prefs.proxy_unsaved = false;
                    return;
                }
            }
        };
        self.prefs.proxy_line = Line::busy(format!("Проверяю {}:{} — отвечает ли OpenRouter…", p.host, p.port));
        self.prefs.proxy_unsaved = false;
        self.net.send(NetCmd::CheckProxy {
            base_url: self.cfg.openrouter.base_url.clone(),
            host: p.host.clone(),
            port: p.port,
            user: p.user.clone(),
            pass: p.pass.clone(),
            kind: p.kind,
        });
        self.prefs.proxy_pending = if saved { None } else { Some(p) };
    }

    /// Пароль — в Credential Manager, остальное — в config.json; маршрут сразу.
    fn save_proxy(&mut self, p: ParsedProxy, kind: ProxyKind) -> bool {
        match &p.pass {
            Some(pass) => {
                if let Err(e) = secrets::write(secrets::PROXY_PASSWORD, p.user.as_deref().unwrap_or(""), pass) {
                    self.prefs.proxy_line = Line::err(e);
                    return false;
                }
            }
            None => secrets::delete(secrets::PROXY_PASSWORD),
        }
        if kind != ProxyKind::Auto {
            self.detected_proxy.insert((p.host.clone(), p.port), kind);
        }
        info!("прокси {}:{} ({kind:?}) сохранён", p.host, p.port);
        let cfg = ProxyConfig { kind, host: p.host, port: p.port, user: p.user };
        self.set(|c| c.proxy = Some(cfg));
        true
    }

    pub(super) fn on_proxy_checked(&mut self, host: String, port: u16, result: Option<(ProxyKind, u64)>) {
        let pending = self.prefs.proxy_pending.take_if(|p| p.host == host && p.port == port);
        match (pending, result) {
            (Some(p), Some((kind, ms))) => {
                let login = p.user.is_some();
                if self.save_proxy(p, kind) {
                    self.prefs.proxy_line = Line::ok(format!("{} — работает, задержка {ms} мс", proxy_words(kind, &host, port, login)));
                    settings::push(Msg::ProxySaved);
                }
            }
            (Some(p), None) => {
                warn!("прокси {host}:{port}: не ответил ни как HTTP, ни как SOCKS");
                self.prefs.proxy_line = Line::err(format!("{host}:{port} не отвечает → проверь адрес, порт и логин"));
                self.prefs.proxy_pending = Some(p);
                self.prefs.proxy_unsaved = true;
            }
            (None, Some((kind, ms))) => {
                let same = self.cfg.proxy.as_ref().is_some_and(|c| c.host == host && c.port == port);
                if same {
                    self.detected_proxy.insert((host.clone(), port), kind);
                    let login = self.cfg.proxy.as_ref().is_some_and(|c| c.user.is_some());
                    self.prefs.proxy_line = Line::ok(format!("{} — работает, задержка {ms} мс", proxy_words(kind, &host, port, login)));
                    if self.cfg.proxy.as_ref().is_some_and(|c| c.kind == ProxyKind::Auto) {
                        self.set(|c| {
                            if let Some(p) = c.proxy.as_mut() {
                                p.kind = kind;
                            }
                        });
                    }
                }
            }
            (None, None) => {
                self.prefs.proxy_line = Line::err(format!("{host}:{port} не отвечает → проверь, жив ли прокси, или замени его"));
            }
        }
        self.publish();
    }

    // ---------- Codex ----------

    fn refresh_codex_login(&mut self) {
        self.prefs.codex_login = match codex::read_login(&codex::auth_file()) {
            Login::Ready(a) => Line::ok(codex::profile_of_token(&a.token).words()),
            Login::None => Line::err(codex::no_login()),
            Login::ApiKey => Line::err(codex::api_key()),
            Login::Broken(why) => Line::err(codex::broken(&why)),
        };
    }

    pub(super) fn on_codex_checked(&mut self, r: Result<codex::Checked, String>) {
        let line = match r {
            Ok(c) => Line::ok(format!(
                "Распознавание отвечает, задержка {}{}",
                sec(c.ms),
                if c.renewed { "; вход Codex обновлён" } else { "" }
            )),
            Err(e) => Line::err(e),
        };
        info!("проверка движка ChatGPT: {}", line.text);
        if !self.prefs.open {
            self.ui.notify(line.text.clone());
        }
        self.prefs.codex_line = line;
        self.publish();
    }

    // ---------- хоткеи ----------

    fn start_capture(&mut self, id: HotkeyId) {
        if self.recording.is_some() {
            self.cancel();
        }
        MATCHER.reset_held();
        self.machine.forget_hold();
        hook::set_capturing(true);
        self.prefs.capture = Some((id, Instant::now()));
        self.prefs.hotkey_warn[row_index(id)].clear();
        info!("захват сочетания: {}", hotkey_title(id, self.cfg.hotkeys.mode).0);
    }

    fn stop_capture(&mut self) {
        if self.prefs.capture.take().is_some() {
            hook::set_capturing(false);
        }
    }

    pub(super) fn capture_tick(&mut self, now: Instant) {
        if self.prefs.capture.is_some_and(|(_, at)| now.duration_since(at) >= CAPTURE_TIMEOUT) {
            self.stop_capture();
            self.publish();
        }
    }

    /// Хук поймал сочетание для строки, которую захватываем.
    pub(super) fn on_captured(&mut self, b: Binding) {
        let Some((id, _)) = self.prefs.capture.take() else { return };
        hook::set_capturing(false);
        let row = row_index(id);
        // Esc без модификаторов — отказ от захвата (кроме самой «Отмены»)
        if b == (Binding { vk: 0x1B, mods: 0 }) && id != HotkeyId::Cancel {
            self.publish();
            return;
        }
        let others = HOTKEY_ROWS.iter().filter(|r| **r != id).find(|r| self.binding(**r) == b);
        if let Some(other) = others {
            self.prefs.hotkey_warn[row] = format!("{} уже на «{}» → выбери другое", b.label(), hotkey_title(*other, self.cfg.hotkeys.mode).0);
            self.publish();
            return;
        }
        let s = b.to_config();
        let r = self.change(|c| match id {
            HotkeyId::Dictate => c.hotkeys.dictate = s,
            HotkeyId::Pause => c.hotkeys.pause = s,
            HotkeyId::PasteLast => c.hotkeys.paste_last = s,
            HotkeyId::Cancel => c.hotkeys.cancel = s,
        });
        match r {
            Ok(()) => {
                info!("сочетание «{}»: {}", hotkey_title(id, self.cfg.hotkeys.mode).0, b.label());
                self.prefs.hotkey_warn[row] = warn_for(id, b);
            }
            Err(e) => self.prefs.hotkey_warn[row] = e,
        }
        self.publish();
    }

    fn refresh_hotkey_warnings(&mut self) {
        for (i, id) in HOTKEY_ROWS.iter().enumerate() {
            self.prefs.hotkey_warn[i] = warn_for(*id, self.binding(*id));
        }
    }

    // ---------- автозапуск ----------

    fn autostart(&mut self, on: bool) {
        if self.prefs.autostart_busy {
            return;
        }
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                self.prefs.autostart_line = Line::err(format!("путь к Verba не узнать: {e}"));
                return;
            }
        };
        self.prefs.autostart_busy = true;
        let elevated = self.prefs.elevated.unwrap_or(false);
        self.prefs.autostart_line = Line::busy(if elevated { "Меняю задачу планировщика…" } else { "Подтверди в окне Windows (UAC)…" });
        let tx = self.ui_tx.clone();
        spawn_worker(tx, "autostart", move || {
            let result = if on { autostart::enable(&exe) } else { autostart::disable() };
            Worker::AutostartDone { on, result }
        });
    }

    fn restart_elevated(&mut self) {
        // новый экземпляр стартует из задачи в трей; метка просит его открыть окно настроек
        if let Err(e) = std::fs::create_dir_all(&self.paths.data_dir).and_then(|_| std::fs::write(show_flag(&self.paths), b"1")) {
            warn!("метка показа окна не записана: {e}");
        }
        self.prefs.autostart_line = Line::busy("Перезапускаю с правами…");
        let tx = self.ui_tx.clone();
        spawn_worker(tx, "restart", || Worker::Restarted(autostart::run_task()));
    }

    // ---------- состояние окна ----------

    fn device_list(&self) -> Vec<String> {
        let mut v = vec!["Как в Windows".to_string()];
        match &self.prefs.devices {
            Some(d) => v.extend(d.iter().cloned()),
            None => {
                if let Some(dev) = &self.cfg.microphone.device {
                    v.push(dev.clone());
                }
            }
        }
        if let Some(dev) = &self.cfg.microphone.device {
            if !v.contains(dev) {
                v.push(dev.clone());
            }
        }
        v
    }

    pub(super) fn settings_view(&self) -> ViewData {
        let p = &self.prefs;
        let names: std::collections::HashMap<&str, &str> =
            p.catalog.iter().flatten().map(|m| (m.id.as_str(), m.name.as_str())).collect();
        let models = self
            .cfg
            .openrouter
            .models
            .iter()
            .map(|id| ModelItem {
                id: id.clone(),
                name: names.get(id.as_str()).map(|n| n.to_string()).unwrap_or_else(|| id.clone()),
                active: *id == self.cfg.openrouter.model,
            })
            .collect();
        let catalog = p
            .catalog
            .iter()
            .flatten()
            .filter(|m| !self.cfg.openrouter.models.contains(&m.id))
            .map(|m| m.id.clone())
            .collect();
        let hotkeys = HOTKEY_ROWS
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let (title, hint) = hotkey_title(*id, self.cfg.hotkeys.mode);
                HotkeyItem {
                    title: title.into(),
                    hint: hint.into(),
                    label: self.binding(*id).label(),
                    capturing: p.capture.is_some_and(|(c, _)| c == *id),
                    warn: p.hotkey_warn[i].clone(),
                }
            })
            .collect();
        let devices = self.device_list();
        let device = self.cfg.microphone.device.as_ref().and_then(|d| devices.iter().position(|x| x == d)).unwrap_or(0);
        let proxy = self
            .cfg
            .proxy
            .as_ref()
            .map(|c| {
                let kind = match c.kind {
                    ProxyKind::Auto => self.detected_proxy.get(&(c.host.clone(), c.port)).copied().unwrap_or(ProxyKind::Auto),
                    k => k,
                };
                proxy_words(kind, &c.host, c.port, c.user.is_some())
            })
            .unwrap_or_default();
        ViewData {
            paused: self.paused,
            dictate_label: self.bindings.dictate.label(),
            codex: self.cfg.engine == Engine::Codex,
            key_mask: p.key_mask.clone().unwrap_or_default(),
            key_line: p.key_line.clone(),
            models,
            catalog,
            catalog_line: p.catalog_line.clone(),
            proxy,
            proxy_line: p.proxy_line.clone(),
            proxy_unsaved: p.proxy_unsaved,
            codex_login: p.codex_login.clone(),
            codex_line: p.codex_line.clone(),
            hotkeys,
            mode: self.cfg.hotkeys.mode,
            hold_ms: self.cfg.hotkeys.hybrid_hold_ms,
            remember_field: self.cfg.paste.remember_field,
            trailing_space: self.cfg.paste.trailing_space,
            humanize: self.cfg.paste.humanize,
            restore_clipboard: self.cfg.paste.restore_clipboard,
            games: self.cfg.paste.games.clone(),
            devices,
            device,
            keep_open: self.cfg.microphone.keep_open,
            snd_rec: self.cfg.sounds.recording,
            snd_pause: self.cfg.sounds.pause,
            snd_err: self.cfg.sounds.errors,
            overlay: self.cfg.overlay,
            autostart: p.autostart,
            autostart_busy: p.autostart_busy,
            autostart_line: p.autostart_line.clone(),
            restart_elevated: p.autostart && !p.autostart_busy && p.elevated == Some(false),
            version: env!("CARGO_PKG_VERSION").into(),
        }
    }
}

/// Задача автозапуска есть и смотрит на этот exe.
fn autostart_state() -> Result<bool, String> {
    let exe = std::env::current_exe().map_err(|e| format!("путь к Verba не узнать: {e}"))?;
    Ok(match autostart::task_state()? {
        autostart::TaskState::Missing => false,
        autostart::TaskState::Present { exe: task_exe, enabled } => enabled && same_file(&task_exe, &exe),
    })
}

fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    let norm = |p: &std::path::Path| p.to_string_lossy().trim_matches('"').replace('/', "\\").to_lowercase();
    norm(a) == norm(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_conflicts_are_named() {
        assert!(common_conflict(Binding::parse("ctrl+c").unwrap()).is_some());
        assert!(common_conflict(Binding::parse("alt+tab").unwrap()).is_some());
        assert!(common_conflict(Binding::parse("win+l").unwrap()).unwrap().contains("Windows"));
        assert!(common_conflict(Binding::parse("ctrl+space").unwrap()).is_none());
        assert!(common_conflict(Binding::parse("ctrl+alt+p").unwrap()).is_none());
    }

    #[test]
    fn plain_letter_warns_but_cancel_does_not() {
        let a = Binding::parse("a").unwrap();
        assert!(warn_for(HotkeyId::Dictate, a).contains("печататься"));
        assert_eq!(warn_for(HotkeyId::Cancel, Binding::parse("esc").unwrap()), "");
    }

    #[test]
    fn proxy_line_words() {
        assert_eq!(proxy_words(ProxyKind::Socks5, "1.2.3.4", 1080, true), "SOCKS5 1.2.3.4:1080 · с логином");
        assert_eq!(proxy_words(ProxyKind::Auto, "h", 1, false), "Прокси h:1");
    }

    #[test]
    fn same_exe_ignores_case_quotes_and_slashes() {
        assert!(same_file(std::path::Path::new("\"C:/Users/X/Desktop/Verba.exe\""), std::path::Path::new("c:\\users\\x\\desktop\\verba.exe")));
        assert!(!same_file(std::path::Path::new("C:\\a\\verba.exe"), std::path::Path::new("C:\\b\\verba.exe")));
    }
}
