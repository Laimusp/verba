//! Контроллер — машина состояний диктовки в своём потоке. Связывает хук, микрофон, сеть,
//! вставку и трей; сам ничего долгого не делает: всё долгое — в потоках-исполнителях, итоги
//! приходят каналами.
//!
//! Диктовка: хоткей → микрофон (команда, не ожидание) + прогрев соединения → отпускание →
//! хвост 100 мс → 16 кГц + VAD → движок с повторами (OpenRouter или ChatGPT по входу Codex) →
//! вставка → строка лога с фазами. Движок берётся при нажатии: переключение в трее действует со
//! следующей диктовки.
//! Запись хранится (в памяти и в `pending.wav`) до успешного распознавания; ошибка — в трей
//! словами «что не так → что сделать»; «вставить последнее» отправляет её снова.
//!
//! Вставка в запомненное поле (`paste.remember_field`): при нажатии — снимок поля (`target`),
//! после распознавания текст уходит в это поле, даже если человек ушёл в другое окно. Пока идёт
//! новая запись, такая вставка ждёт её конца: фокус во время диктовки не трогаем.
//!
//! Окно настроек (`prefs.rs`): его действия приходят сюда же, контроллер меняет настройки, пишет
//! config.json и шлёт окну новое состояние — изменения действуют сразу, без перезапуска.
//! Оверлей записи — плашка внизу экрана: запись → распознаю → скрыта или ошибка словами.

mod prefs;

pub use prefs::{show_flag, Worker};

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{select, Receiver, Sender};

use crate::audio::recorder::{AudioCmd, AudioEvent};
use crate::config::{self, Bindings, Config, Engine, Paths, ProxyKind};
use crate::hotkey::binding::{Binding, MOD_ALT, MOD_WIN};
use crate::hotkey::hook::{self, HookEvent, HookThread, HotkeyId, MATCHER};
use crate::hotkey::mode::{Action, ModeMachine};
use crate::net::codex::{self, CodexSetup};
use crate::net::service::{Job, NetCmd, NetEvent, NetService};
use crate::net::words::Service;
use crate::net::{proxy, timing_words, words, Route, Warm};
use crate::overlay::{Overlay, Position};
use crate::paste::{self, PasteReport};
use crate::secrets;
use crate::settings::{self, ViewData};
use crate::sound::{self, Cue};
use crate::target::{self, Delivered, How, Snapshot, TargetService, Why};
use crate::tray::{MenuCmd, Status, UiEvent, UiHandle};
use crate::{error, info, warn};

/// Удержание считается потерянным, если модификаторы основного сочетания отпущены дольше этого,
/// а отпускания хук так и не увидел (Win+L, окно с правами администратора).
const LOST_RELEASE_AFTER: Duration = Duration::from_millis(300);
/// Медленная диктовка — отдельной строкой лога с разбором (CLAUDE.md §6).
const SLOW: Duration = Duration::from_secs(3);

pub struct Channels {
    pub hook: Receiver<HookEvent>,
    pub audio: Receiver<AudioEvent>,
    pub net: Receiver<NetEvent>,
    pub ui: Receiver<UiEvent>,
    pub paste: Receiver<(u64, PasteReport)>,
    pub target: Receiver<Delivered>,
    /// сочетание, пойманное хуком при захвате в окне настроек
    pub capture: Receiver<Binding>,
}

#[derive(Default)]
struct Dictation {
    id: u64,
    /// чем распознаётся: берётся при нажатии хоткея (или при повторе)
    engine: Engine,
    pressed_at: Option<Instant>,
    released_at: Option<Instant>,
    /// от нажатия до первого звука, мс; из них открытие устройства — `mic_device_ms`
    mic_open_ms: Option<u64>,
    mic_device_ms: u64,
    device: String,
    /// вставлять ли результат (Esc во время распознавания, блокировка экрана — нет)
    paste: bool,
    /// повтор сохранённой записи по «вставить последнее»
    retry: bool,
    total_ms: u64,
    sent_ms: u64,
    speech_ms: u64,
    encode_ms: u64,
    wav: Option<Arc<Vec<u8>>>,
    sent_at: Option<Instant>,
    warm: Option<Warm>,
    net_ms: Option<u64>,
    tries: usize,
    net_detail: String,
    /// что было со входом Codex (обновление, повторная отправка)
    login_detail: String,
    paste_started: Option<Instant>,
    /// поле, где был курсор при нажатии (вставка в запомненное поле)
    target: Option<Arc<Snapshot>>,
    /// как доставлено в запомненное поле — для строки лога
    delivery: Option<String>,
    /// от начала доставки до Ctrl+V или вставки, мс
    delivery_ms: Option<u64>,
    /// фокус возвращали: непрочитанный буфер значит, что текст не вставлен
    restored_focus: bool,
}

pub struct Controller {
    paths: Paths,
    cfg: Config,
    bindings: Bindings,
    cfg_modified: Option<SystemTime>,
    epoch: Instant,
    machine: ModeMachine,
    paused: bool,
    locked: bool,
    recording: Option<(u64, Instant)>,
    dictations: HashMap<u64, Dictation>,
    pasting: HashMap<u64, Dictation>,
    /// распознано во время новой записи — вставить после неё (id, текст)
    deferred: VecDeque<(u64, String)>,
    next_id: u64,
    history: VecDeque<String>,
    /// последняя нераспознанная запись
    pending: Option<Arc<Vec<u8>>>,
    suppressed_while_locked: bool,
    error: Option<String>,
    has_key: bool,
    mods_released_since: Option<Instant>,
    detected_proxy: HashMap<(String, u16), ProxyKind>,
    last_cfg_check: Instant,
    ui: UiHandle,
    net: NetService,
    audio: Sender<AudioCmd>,
    hook: HookThread,
    paste_tx: Sender<(u64, PasteReport)>,
    targets: TargetService,
    overlay: Plate,
    /// канал событий UI: им же возвращаются итоги фоновой работы (`Worker`)
    ui_tx: Sender<UiEvent>,
    /// состояние окна настроек
    prefs: prefs::Prefs,
    /// последнее, что ушло окну настроек: одинаковое не шлём
    shown_view: RefCell<Option<ViewData>>,
}

/// Плашка записи; оверлей не запустился — все вызовы ничего не делают.
struct Plate(Option<Overlay>);

impl Plate {
    fn set_position(&self, p: Position) {
        if let Some(o) = &self.0 {
            o.set_position(p);
        }
    }
    fn recording(&self) {
        if let Some(o) = &self.0 {
            o.recording();
        }
    }
    fn transcribing(&self) {
        if let Some(o) = &self.0 {
            o.transcribing();
        }
    }
    fn error(&self, text: &str) {
        if let Some(o) = &self.0 {
            o.error(text);
        }
    }
    fn flash(&self, text: &str) {
        if let Some(o) = &self.0 {
            o.flash(text);
        }
    }
    fn hide(&self) {
        if let Some(o) = &self.0 {
            o.hide();
        }
    }
    fn works(&self) -> bool {
        self.0.is_some()
    }
}

fn sec(ms: u64) -> String {
    format!("{:.2} с", ms as f64 / 1000.0).replace('.', ",")
}

fn ms_between(a: Instant, b: Instant) -> u64 {
    b.saturating_duration_since(a).as_millis() as u64
}

pub struct Parts {
    pub paths: Paths,
    pub cfg: Config,
    pub bindings: Bindings,
    pub cfg_modified: Option<SystemTime>,
    pub ui: UiHandle,
    pub net: NetService,
    pub audio: Sender<AudioCmd>,
    pub hook: HookThread,
    pub paste_tx: Sender<(u64, PasteReport)>,
    pub targets: TargetService,
    /// None — оверлей не запустился: ошибки уходят уведомлениями трея
    pub overlay: Option<Overlay>,
    pub ui_tx: Sender<UiEvent>,
    pub epoch: Instant,
}

impl Controller {
    pub fn new(p: Parts) -> Self {
        let machine = ModeMachine::new(p.cfg.hotkeys.mode, p.cfg.hotkeys.hybrid_hold_ms);
        let pending = std::fs::read(p.paths.pending_file()).ok().filter(|w| w.len() > 44).map(Arc::new);
        let mut c = Controller {
            paths: p.paths,
            cfg: p.cfg,
            bindings: p.bindings,
            cfg_modified: p.cfg_modified,
            epoch: p.epoch,
            machine,
            paused: false,
            locked: false,
            recording: None,
            dictations: HashMap::new(),
            pasting: HashMap::new(),
            deferred: VecDeque::new(),
            next_id: 1,
            history: VecDeque::new(),
            pending,
            suppressed_while_locked: false,
            error: None,
            has_key: secrets::read(secrets::OPENROUTER_KEY).is_some(),
            mods_released_since: None,
            detected_proxy: HashMap::new(),
            last_cfg_check: Instant::now(),
            ui: p.ui,
            net: p.net,
            audio: p.audio,
            hook: p.hook,
            paste_tx: p.paste_tx,
            targets: p.targets,
            overlay: Plate(p.overlay),
            ui_tx: p.ui_tx,
            prefs: prefs::Prefs::default(),
            shown_view: RefCell::new(None),
        };
        c.overlay.set_position(c.cfg.overlay);
        c.prefs.key_mask = secrets::read(secrets::OPENROUTER_KEY).map(|k| settings::mask_key(&k));
        if c.pending.is_some() {
            info!("есть нераспознанная запись с прошлого раза — «вставить последнее» отправит её");
        }
        if c.cfg.engine == Engine::OpenRouter && !c.has_key {
            c.ui.notify(words::no_key());
        }
        c.apply_route();
        c.publish();
        c
    }

    pub fn run(mut self, ch: Channels) {
        loop {
            let wait = if self.recording.is_some() { Duration::from_millis(100) } else { Duration::from_secs(3) };
            select! {
                recv(ch.hook) -> m => match m {
                    Ok(e) => self.on_hotkey(e),
                    Err(_) => return,
                },
                recv(ch.audio) -> m => if let Ok(e) = m { self.on_audio(e) },
                recv(ch.net) -> m => if let Ok(e) = m { self.on_net(e) },
                recv(ch.ui) -> m => match m {
                    Ok(UiEvent::Menu(MenuCmd::Exit)) | Ok(UiEvent::Settings(settings::Cmd::Exit)) | Err(_) => {
                        self.shutdown();
                        return;
                    }
                    Ok(e) => self.on_ui(e),
                },
                recv(ch.paste) -> m => if let Ok((id, r)) = m { self.on_paste(id, r) },
                recv(ch.target) -> m => if let Ok(d) = m { self.on_target(d) },
                recv(ch.capture) -> m => if let Ok(b) = m { self.on_captured(b) },
                default(wait) => {}
            }
            self.tick();
        }
    }

    fn shutdown(&mut self) {
        if let Some((id, _)) = self.recording.take() {
            let _ = self.audio.send(AudioCmd::Cancel { id });
        }
        hook::set_capturing(false);
        self.overlay.hide();
        info!("Verba закрыта");
        self.ui.quit();
    }

    fn now_ms(&self, at: Instant) -> u64 {
        ms_between(self.epoch, at)
    }

    // ---------- хоткеи ----------

    fn binding(&self, id: HotkeyId) -> Binding {
        match id {
            HotkeyId::Dictate => self.bindings.dictate,
            HotkeyId::Cancel => self.bindings.cancel,
            HotkeyId::Pause => self.bindings.pause,
            HotkeyId::PasteLast => self.bindings.paste_last,
        }
    }

    /// Проглоченное сочетание с Alt или Win: без маски отпускание Alt откроет меню окна.
    fn mask_menu(&self, id: HotkeyId) {
        if self.binding(id).mods & (MOD_ALT | MOD_WIN) != 0 {
            hook::inject_menu_mask();
        }
    }

    fn on_hotkey(&mut self, e: HookEvent) {
        let ms = self.now_ms(e.at);
        match (e.id, e.pressed) {
            (HotkeyId::Dictate, true) => {
                self.mask_menu(e.id);
                if self.paused {
                    return;
                }
                match self.machine.press(ms) {
                    Action::Start => self.start(e.at),
                    Action::Stop => self.stop(),
                    Action::None => {}
                }
            }
            (HotkeyId::Dictate, false) => {
                if self.machine.release(ms) == Action::Stop {
                    self.stop();
                }
            }
            (HotkeyId::Cancel, true) => self.cancel(),
            (HotkeyId::Pause, true) => {
                self.mask_menu(e.id);
                self.toggle_pause();
            }
            (HotkeyId::PasteLast, true) => {
                self.mask_menu(e.id);
                self.paste_last();
            }
            _ => {}
        }
    }

    fn start(&mut self, pressed_at: Instant) {
        self.reload_if_changed();
        let id = self.next_id;
        self.next_id += 1;
        // первым — микрофон: каждая миллисекунда до его открытия — потерянный звук
        let _ = self.audio.send(AudioCmd::Start { id });
        // вторым — снимок поля: человек ещё здесь, фокус на месте
        let target = self.capture_target();
        self.recording = Some((id, pressed_at));
        self.mods_released_since = None;
        let engine = self.cfg.engine;
        self.dictations
            .insert(id, Dictation { id, engine, pressed_at: Some(pressed_at), paste: !self.locked, target, ..Default::default() });
        MATCHER.set_cancel_armed(true);
        match engine {
            Engine::OpenRouter => {
                let key = secrets::read(secrets::OPENROUTER_KEY);
                self.has_key = key.is_some();
                if key.is_some() {
                    self.net.send(NetCmd::Warm { engine, key });
                }
            }
            // вход читается при отправке: прогреву он не нужен — chatgpt.com отвечает без входа
            Engine::Codex => self.net.send(NetCmd::Warm { engine, key: None }),
        }
        if self.cfg.sounds.recording {
            sound::play(Cue::Start);
        }
        self.overlay.recording();
        self.publish();
    }

    fn stop(&mut self) {
        let Some((id, _)) = self.recording.take() else { return };
        MATCHER.set_cancel_armed(false);
        if let Some(d) = self.dictations.get_mut(&id) {
            d.released_at = Some(Instant::now());
        }
        let _ = self.audio.send(AudioCmd::Stop {
            id,
            tail_ms: self.cfg.recording.tail_ms,
            trim_silence: self.cfg.recording.trim_silence,
        });
        if self.cfg.sounds.recording {
            sound::play(Cue::Stop);
        }
        self.overlay.transcribing();
        self.publish();
        self.flush_deferred();
    }

    fn cancel(&mut self) {
        let Some((id, _)) = self.recording.take() else { return };
        MATCHER.set_cancel_armed(false);
        let _ = self.audio.send(AudioCmd::Cancel { id });
        self.dictations.remove(&id);
        self.machine.reset();
        info!("диктовка #{id}: отменена");
        self.overlay_idle();
        self.publish();
        self.flush_deferred();
    }

    /// Плашка после записи: идёт распознавание — «распознаю», иначе скрыть.
    fn overlay_idle(&self) {
        if self.recording.is_some() {
            return;
        }
        if self.dictations.values().any(|d| d.sent_at.is_some() || d.released_at.is_some()) {
            self.overlay.transcribing();
        } else {
            self.overlay.hide();
        }
    }

    fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        MATCHER.set_paused(self.paused);
        if self.paused {
            self.cancel();
        }
        self.overlay.flash(if self.paused { "Пауза" } else { "Слушаю" });
        if self.cfg.sounds.pause {
            sound::play(if self.paused { Cue::PauseOn } else { Cue::PauseOff });
        }
        info!("{}", if self.paused { "пауза: основной хоткей уходит в приложения" } else { "пауза снята" });
        self.publish();
    }

    fn paste_last(&mut self) {
        if let Some(wav) = self.pending.clone() {
            let busy = self.dictations.values().any(|d| d.wav.as_ref().is_some_and(|w| Arc::ptr_eq(w, &wav)));
            if busy {
                self.ui.notify("Повтор уже идёт — дождись ответа");
                return;
            }
            let id = self.next_id;
            self.next_id += 1;
            info!("диктовка #{id}: повтор сохранённой записи по «вставить последнее»");
            // текст встанет туда, где курсор был при нажатии «вставить последнее»
            let target = self.capture_target();
            self.dictations.insert(
                id,
                Dictation {
                    id,
                    engine: self.cfg.engine,
                    paste: !self.locked,
                    retry: true,
                    wav: Some(wav.clone()),
                    target,
                    ..Default::default()
                },
            );
            self.transcribe(id, wav);
            self.publish();
        } else if let Some(text) = self.history.front().cloned() {
            self.paste_text(0, text, None);
        } else {
            self.ui.notify("Вставлять нечего: диктовок ещё не было");
        }
    }

    // ---------- микрофон ----------

    fn on_audio(&mut self, e: AudioEvent) {
        match e {
            AudioEvent::Started { id, open_ms, device } => {
                if let Some(d) = self.dictations.get_mut(&id) {
                    d.mic_open_ms = d.pressed_at.map(|p| ms_between(p, Instant::now()));
                    d.mic_device_ms = open_ms;
                    d.device = device;
                }
            }
            AudioEvent::Failed { id, words } => {
                if self.recording.is_some_and(|(r, _)| r == id) {
                    self.recording = None;
                    MATCHER.set_cancel_armed(false);
                    self.machine.reset();
                }
                self.dictations.remove(&id);
                self.fail(id, words);
                self.flush_deferred();
            }
            AudioEvent::Finished { id, processed, encode_ms, device_lost } => {
                let Some(d) = self.dictations.get_mut(&id) else { return };
                d.total_ms = processed.total_ms;
                d.sent_ms = processed.sent_ms;
                d.speech_ms = processed.speech_ms;
                d.encode_ms = encode_ms;
                if device_lost {
                    warn!("диктовка #{id}: микрофон отключился посреди записи — отправляю то, что записано");
                }
                if self.cfg.recording.trim_silence && processed.speech_ms == 0 {
                    let mic = match d.mic_open_ms {
                        Some(ms) => format!("; хоткей→микрофон {ms} мс (открытие {} мс, {})", d.mic_device_ms, d.device),
                        None => String::new(),
                    };
                    self.dictations.remove(&id);
                    info!("диктовка #{id}: речи нет (запись {}{mic}) — не отправлял", sec(processed.total_ms));
                    if processed.total_ms >= 1500 {
                        self.show_error("Речи не слышно → проверь микрофон: выбран тот и не выключен ли звук");
                    } else {
                        self.overlay_idle();
                    }
                    self.publish();
                    return;
                }
                let wav = Arc::new(processed.wav);
                d.wav = Some(wav.clone());
                self.transcribe(id, wav.clone());
                self.set_pending(Some(wav));
            }
        }
    }

    fn transcribe(&mut self, id: u64, wav: Arc<Vec<u8>>) {
        let engine = self.dictations.get(&id).map(|d| d.engine).unwrap_or(self.cfg.engine);
        let job = match engine {
            Engine::OpenRouter => {
                let Some(key) = secrets::read(secrets::OPENROUTER_KEY) else {
                    self.has_key = false;
                    if let Some(d) = self.dictations.remove(&id) {
                        self.log_timing(&d, None);
                    }
                    self.fail(id, words::no_key());
                    return;
                };
                Job::OpenRouter { key, model: self.cfg.openrouter.model.clone() }
            }
            // auth.json читает сетевой поток: диск — не здесь
            Engine::Codex => Job::Codex,
        };
        if let Some(d) = self.dictations.get_mut(&id) {
            d.sent_at = Some(Instant::now());
        }
        self.net.send(NetCmd::Transcribe { id, job, wav });
    }

    fn set_pending(&mut self, wav: Option<Arc<Vec<u8>>>) {
        let path = self.paths.pending_file();
        match &wav {
            Some(w) => {
                let tmp = path.with_extension("wav.tmp");
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                if let Err(e) = std::fs::write(&tmp, w.as_slice()).and_then(|_| std::fs::rename(&tmp, &path)) {
                    warn!("запись не сохранена на диск ({e}) — держу в памяти");
                }
            }
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
        self.pending = wav;
    }

    // ---------- сеть ----------

    fn on_net(&mut self, e: NetEvent) {
        match e {
            NetEvent::Warmed(w) => {
                if let Some(err) = &w.error {
                    warn!("прогрев соединения не удался: {err}");
                } else if w.replaced {
                    warn!("прогрев: первое соединение молчало — открыто новое ({})", sec(w.ms));
                }
                let target = self.recording.map(|(id, _)| id).or_else(|| self.dictations.keys().max().copied());
                if let Some(d) = target.and_then(|id| self.dictations.get_mut(&id)) {
                    d.warm = Some(w);
                }
            }
            NetEvent::Transcribed { id, engine, result, sent_at, trace } => {
                let Some(mut d) = self.dictations.remove(&id) else { return };
                d.sent_at = Some(sent_at);
                d.login_detail = codex::trace_words(&trace);
                match result {
                    Ok(a) => {
                        d.tries = a.attempts.len();
                        d.net_ms = Some(a.ms);
                        d.net_detail = timing_words(&a.attempts);
                        if a.status == 200 {
                            match a.text() {
                                Some(t) if !t.is_empty() => self.succeed(d, t),
                                _ => {
                                    self.log_timing(&d, None);
                                    self.fail(id, "модель вернула пустой текст → скажи ещё раз или «вставить последнее»".into());
                                }
                            }
                        } else {
                            self.log_timing(&d, None);
                            let words = match engine {
                                Engine::OpenRouter => {
                                    if a.status == 401 || a.status == 403 {
                                        self.has_key = secrets::read(secrets::OPENROUTER_KEY).is_some();
                                    }
                                    words::refusal(a.status, &words::or_message(a.json.as_ref()), &self.cfg.openrouter.model)
                                }
                                Engine::Codex => codex::answer_words(&a, &trace),
                            };
                            self.fail(id, words);
                        }
                    }
                    Err(f) => {
                        d.tries = f.attempts().len();
                        d.net_detail = timing_words(f.attempts());
                        self.log_timing(&d, None);
                        let service = match engine {
                            Engine::OpenRouter => Service::OpenRouter,
                            Engine::Codex => Service::ChatGpt,
                        };
                        self.fail(id, f.words_at(service, self.cfg.proxy.is_some()));
                    }
                }
                self.publish();
            }
            NetEvent::KeyChecked(r) => self.on_key_checked(r),
            NetEvent::CodexChecked(r) => self.on_codex_checked(r),
            NetEvent::Models(r) => self.on_models(r),
            NetEvent::ProxyChecked { host, port, result } => self.on_proxy_checked(host, port, result),
            NetEvent::ProxyDetected { host, port, kind } => {
                let same = self.cfg.proxy.as_ref().is_some_and(|p| p.host == host && p.port == port);
                match kind {
                    Some(k) => {
                        info!("прокси {host}:{port}: вид {k:?}");
                        self.detected_proxy.insert((host.clone(), port), k);
                        if same {
                            if let Some(p) = self.cfg.proxy.as_mut() {
                                p.kind = k;
                            }
                            self.save_config();
                            self.apply_route();
                            self.ui.notify(format!("Прокси {host}:{port} работает ({k:?})"));
                        }
                    }
                    None => {
                        warn!("прокси {host}:{port}: не ответил ни как HTTP, ни как SOCKS");
                        self.ui.notify(format!("Прокси {host}:{port} не отвечает → проверь адрес, порт и логин"));
                    }
                }
            }
            NetEvent::RouteError(e) => {
                error!("{e}");
                self.ui.notify(e);
            }
        }
    }

    fn succeed(&mut self, d: Dictation, text: String) {
        if self.pending.as_ref().zip(d.wav.as_ref()).is_some_and(|(p, w)| Arc::ptr_eq(p, w)) {
            self.set_pending(None);
        }
        self.error = None;
        self.history.push_front(text.clone());
        self.history.truncate(self.cfg.history_size);
        if d.paste && !self.locked {
            let id = d.id;
            let mut d = d;
            d.paste_started = Some(Instant::now());
            let target = d.target.clone();
            self.pasting.insert(id, d);
            if self.recording.is_some() && self.remember_field(&target) {
                info!("диктовка #{id}: распознана во время новой записи — вставлю после неё");
                self.deferred.push_back((id, text));
            } else {
                self.paste_text(id, text, target);
            }
        } else {
            if self.locked {
                self.suppressed_while_locked = true;
            }
            info!("диктовка #{}: распознана, не вставлена (отмена или экран заблокирован)", d.id);
            self.log_timing(&d, None);
        }
        self.overlay_idle();
        self.publish();
    }

    /// Снимок поля в фокусе, если вставка в запомненное поле включена.
    fn capture_target(&self) -> Option<Arc<Snapshot>> {
        self.cfg.paste.remember_field.then(|| self.targets.capture(&self.cfg.paste.games))
    }

    fn remember_field(&self, target: &Option<Arc<Snapshot>>) -> bool {
        target.is_some() && self.cfg.paste.remember_field
    }

    /// Вставки, отложенные на время записи.
    fn flush_deferred(&mut self) {
        if self.recording.is_some() {
            return;
        }
        while let Some((id, text)) = self.deferred.pop_front() {
            if self.locked {
                self.suppressed_while_locked = true;
                if let Some(d) = self.pasting.remove(&id) {
                    info!("диктовка #{id}: не вставлена — экран заблокирован");
                    self.log_timing(&d, None);
                }
                continue;
            }
            let target = self.pasting.get(&id).and_then(|d| d.target.clone());
            self.paste_text(id, text, target);
        }
    }

    fn paste_text(&mut self, id: u64, mut text: String, target: Option<Arc<Snapshot>>) {
        if self.cfg.paste.trailing_space && !text.ends_with(char::is_whitespace) {
            text.push(' ');
        }
        let tx = self.paste_tx.clone();
        let done = move |r| {
            let _ = tx.send((id, r));
        };
        match target.filter(|_| self.cfg.paste.remember_field) {
            Some(snapshot) => self.targets.deliver(target::Job {
                id,
                text,
                snapshot,
                method: self.cfg.paste.method,
                restore_clipboard: self.cfg.paste.restore_clipboard,
                games: self.cfg.paste.games.clone(),
                done: Box::new(done),
            }),
            None => paste::paste(text, self.cfg.paste.method, self.cfg.paste.restore_clipboard, done),
        }
    }

    fn on_target(&mut self, e: Delivered) {
        let Delivered { id, how, detail, ms } = e;
        let Some(d) = self.pasting.get_mut(&id) else {
            // итог вставки пришёл раньше (ошибка буфера) — строка лога уже есть
            info!("вставка #{id}: {detail}");
            return;
        };
        d.delivery = Some(detail);
        d.delivery_ms = Some(ms);
        match how {
            How::Same | How::PasteError => {}
            How::Restored { .. } => d.restored_focus = true,
            How::ReplaceSel => {
                if let Some(d) = self.pasting.remove(&id) {
                    self.log_timing(&d, None);
                }
            }
            How::NotPasted(why) => {
                if let Some(d) = self.pasting.remove(&id) {
                    self.log_timing(&d, None);
                }
                self.fail(id, why.words(&self.bindings.paste_last.label()));
            }
        }
    }

    fn on_paste(&mut self, id: u64, r: PasteReport) {
        let restored_focus = self.pasting.get(&id).is_some_and(|d| d.restored_focus);
        if let Some(d) = self.pasting.remove(&id) {
            self.log_timing(&d, Some(&r));
        }
        if r.aborted {
            return;
        }
        if let Some(e) = r.error {
            self.fail(id, e);
        } else if r.read_ms.is_none() && restored_focus {
            // окно вернули вперёд и нажали Ctrl+V, а приложение буфер не прочитало — текста в поле нет
            self.fail(id, Why::NotRead.words(&self.bindings.paste_last.label()));
        } else if r.read_ms.is_none() && self.cfg.paste.restore_clipboard {
            info!(
                "вставка #{id}: приложение не прочитало буфер за 8 с — фокус не в текстовом поле или окно запущено                  с правами администратора (Verba без прав в него не пишет); текст — в «Последних диктовках»"
            );
        }
    }

    fn fail(&mut self, id: u64, words: String) {
        error!("диктовка #{id}: {words}");
        self.error = Some(words.clone());
        self.show_error(&words);
        if self.cfg.sounds.errors {
            sound::play(Cue::Error);
        }
        self.publish();
    }

    /// Ошибка словами: на плашке записи, если она включена (и не мешает идущей записи), иначе —
    /// уведомлением трея. Значок трея держит ошибку до удачной диктовки в любом случае.
    fn show_error(&self, words: &str) {
        if self.overlay.works() && self.cfg.overlay != Position::Off && self.recording.is_none() {
            self.overlay.error(words);
        } else {
            self.ui.notify(words.to_string());
        }
    }

    /// Строка лога с фазами (CLAUDE.md §6): без текста диктовки и ключей.
    fn log_timing(&self, d: &Dictation, paste: Option<&PasteReport>) {
        let mut parts = vec![d.engine.label().to_string()];
        if let Some(ms) = d.mic_open_ms {
            let dev = if d.device.is_empty() { "?" } else { &d.device };
            parts.push(format!("хоткей→микрофон {ms} мс (открытие {} мс, {dev})", d.mic_device_ms));
        }
        if d.retry {
            parts.push("повтор сохранённой записи".into());
        } else {
            parts.push(format!("запись {}, речь {}, ушло {}", sec(d.total_ms), sec(d.speech_ms), sec(d.sent_ms)));
            parts.push(format!("обработка {} мс", d.encode_ms));
        }
        if let Some(w) = &d.wav {
            parts.push(format!("{} КБ", w.len() / 1024));
        }
        match &d.warm {
            Some(Warm { connect_ms: Some(c), .. }) => parts.push(format!("прогрев: новое соединение {}", sec(*c))),
            Some(Warm { error: Some(e), .. }) => parts.push(format!("прогрев не удался: {e}")),
            Some(_) => parts.push("прогрев: соединение живое".into()),
            None => {}
        }
        if !d.login_detail.is_empty() {
            parts.push(d.login_detail.clone());
        }
        if let Some(ms) = d.net_ms {
            parts.push(format!("сеть {} ({} отпр.)", sec(ms), d.tries.max(1)));
        }
        if let Some(t) = &d.target {
            parts.push(format!("снимок поля {} мкс", t.capture_us));
        }
        if let Some(how) = &d.delivery {
            parts.push(format!("запомненное поле: {how}"));
        }
        // от отпускания (или от повторной отправки) до Ctrl+V; доставка в запомненное поле —
        // её время до Ctrl+V плюс время Ctrl+V внутри вставки
        let from = d.released_at.or(d.sent_at);
        let mut total = None;
        if let Some(r) = paste {
            parts.push(format!(
                "вставка: Ctrl+V через {} мс, прочитано {}",
                r.chord_ms,
                r.read_ms.map(|m| format!("через {m} мс")).unwrap_or_else(|| "не было".into())
            ));
        }
        let insert_ms = match (paste, d.delivery_ms) {
            (Some(r), Some(dm)) => Some(dm + r.chord_ms),
            (Some(r), None) => Some(r.chord_ms),
            (None, Some(dm)) if d.delivery.is_some() => Some(dm),
            _ => None,
        };
        if let (Some(f), Some(ps), Some(ins)) = (from, d.paste_started, insert_ms) {
            let ms = ms_between(f, ps) + ins;
            total = Some(ms);
            let from = if d.retry { "от повторной отправки" } else { "от отпускания" };
            parts.push(format!("{from} до вставки {}", sec(ms)));
        }
        info!("диктовка #{}: {}", d.id, parts.join("; "));
        let slow = total.is_some_and(|t| t > SLOW.as_millis() as u64);
        if slow || d.tries > 1 || !d.login_detail.is_empty() {
            let what = if slow {
                "медленная"
            } else if d.tries > 1 {
                "с повтором"
            } else {
                "с обновлением входа"
            };
            let warm = match &d.warm {
                Some(w) => format!(
                    "; прогрев {} ({}{})",
                    sec(w.ms),
                    w.status.map(|s| s.to_string()).unwrap_or_else(|| "без ответа".into()),
                    if w.replaced { ", клиент заменён" } else { "" }
                ),
                None => String::new(),
            };
            warn!("диктовка #{} {what}: {}{warm}", d.id, d.net_detail);
        }
    }

    // ---------- трей и сессия ----------

    fn on_ui(&mut self, e: UiEvent) {
        match e {
            UiEvent::Menu(cmd) => match cmd {
                MenuCmd::TogglePause => self.toggle_pause(),
                MenuCmd::SelectModel(i) => {
                    if let Some(m) = self.cfg.openrouter.models.get(i).cloned() {
                        // модель OpenRouter выбирает и сам движок
                        info!("движок OpenRouter, модель: {m}");
                        self.set(|c| {
                            c.openrouter.model = m;
                            c.engine = Engine::OpenRouter;
                        });
                    }
                }
                MenuCmd::SelectEngine(engine) => self.select_engine(engine),
                MenuCmd::CopyHistory(i) => {
                    if let Some(t) = self.history.get(i) {
                        if !paste::copy_text(t) {
                            self.ui.notify("Буфер обмена занят другой программой → попробуй ещё раз");
                        }
                    }
                }
                MenuCmd::Exit => {}
            },
            UiEvent::Settings(cmd) => self.on_settings(cmd),
            UiEvent::Worker(w) => self.on_worker(w),
            UiEvent::Lock => {
                self.locked = true;
                // отпускание сочетания на экране блокировки хук не увидит — запись не должна висеть
                if self.recording.is_some() {
                    info!("экран заблокирован во время записи — останавливаю, текст не вставлю");
                    self.machine.reset();
                    self.stop();
                }
                for d in self.dictations.values_mut() {
                    d.paste = false;
                }
            }
            UiEvent::Unlock | UiEvent::Resume => {
                if matches!(e, UiEvent::Unlock) {
                    self.locked = false;
                }
                self.hook.reinstall();
                self.machine.forget_hold();
                if self.recording.is_none() {
                    MATCHER.reset_held();
                }
                if self.suppressed_while_locked && !self.locked {
                    self.suppressed_while_locked = false;
                    self.ui.notify(format!(
                        "Пока экран был заблокирован, текст не вставлялся → «вставить последнее» ({})",
                        self.bindings.paste_last.label()
                    ));
                }
            }
        }
    }

    fn select_engine(&mut self, engine: Engine) {
        if engine == self.cfg.engine {
            return;
        }
        info!("движок: {} — со следующей диктовки", engine.label());
        self.set(|c| c.engine = engine);
        if engine == Engine::OpenRouter && !self.has_key && !self.prefs.open {
            self.ui.notify(words::no_key());
        }
    }

    // ---------- настройки ----------

    /// Адрес прокси с паролем; вид не известен — запуск определения и пока HTTP.
    fn proxy_url(&mut self) -> Option<String> {
        let base_url = self.cfg.openrouter.base_url.clone();
        let p = self.cfg.proxy.clone()?;
        let pass = secrets::read(secrets::PROXY_PASSWORD);
        let kind = match p.kind {
            ProxyKind::Auto => match self.detected_proxy.get(&(p.host.clone(), p.port)) {
                Some(k) => *k,
                None => {
                    self.net.send(NetCmd::DetectProxy {
                        base_url: base_url.clone(),
                        host: p.host.clone(),
                        port: p.port,
                        user: p.user.clone(),
                        pass: pass.clone(),
                    });
                    // пока вид не известен — HTTP: самый частый
                    ProxyKind::Http
                }
            },
            k => k,
        };
        Some(proxy::proxy_url(kind, &p.host, p.port, p.user.as_deref(), pass.as_deref()))
    }

    /// Маршруты обоих движков: прокси общий.
    fn apply_route(&mut self) {
        let proxy = self.proxy_url();
        self.net.send(NetCmd::SetRoute(Route { base_url: self.cfg.openrouter.base_url.clone(), proxy: proxy.clone() }));
        self.net.send(NetCmd::SetCodex(CodexSetup {
            base_url: self.cfg.codex.base_url.clone(),
            proxy,
            auth_file: codex::auth_file(),
            program: self.cfg.codex.program.as_ref().map(std::path::PathBuf::from),
        }));
    }

    fn save_config(&mut self) {
        let path = self.paths.config_file();
        match config::save(&path, &self.cfg) {
            Ok(()) => self.cfg_modified = config::modified(&path),
            Err(e) => self.ui.notify(format!("настройки не сохранены: {e} → проверь права на {}", path.display())),
        }
    }

    /// Изменение из окна настроек или меню трея: проверить, применить сразу, записать в config.json.
    /// Ошибка проверки (два одинаковых сочетания) — слова для человека, настройки прежние.
    fn change(&mut self, f: impl FnOnce(&mut Config)) -> Result<(), String> {
        let mut new = self.cfg.clone();
        f(&mut new);
        let bindings = new.validate()?;
        self.adopt(new, bindings);
        self.save_config();
        Ok(())
    }

    /// `change` из меню трея: ошибка — уведомлением.
    fn set(&mut self, f: impl FnOnce(&mut Config)) {
        if let Err(e) = self.change(f) {
            warn!("{e}");
            self.ui.notify(e);
        }
    }

    /// Новые настройки — в дело без перезапуска: хоткеи, режим, микрофон, маршрут, плашка.
    fn adopt(&mut self, new: Config, bindings: Bindings) {
        let route_changed =
            new.proxy != self.cfg.proxy || new.openrouter.base_url != self.cfg.openrouter.base_url || new.codex != self.cfg.codex;
        let mic_changed = new.microphone != self.cfg.microphone;
        let keys_changed = new.hotkeys != self.cfg.hotkeys;
        if keys_changed && self.recording.is_some() {
            // режим и сочетания меняются только между записями — иначе отпускание потеряет смысл
            self.cancel();
        }
        let overlay = new.overlay;
        self.cfg = new;
        self.bindings = bindings;
        MATCHER.set_bindings(&self.bindings);
        self.machine.configure(self.cfg.hotkeys.mode, self.cfg.hotkeys.hybrid_hold_ms);
        if mic_changed {
            let _ = self.audio.send(AudioCmd::Configure(self.cfg.microphone.clone()));
        }
        if route_changed {
            self.apply_route();
        }
        self.overlay.set_position(overlay);
        self.publish();
    }

    fn reload_if_changed(&mut self) {
        let path = self.paths.config_file();
        let m = config::modified(&path);
        if m.is_none() || m == self.cfg_modified {
            return;
        }
        self.cfg_modified = m;
        match config::load(&path) {
            Ok(l) => {
                if self.recording.is_some() {
                    // режим хоткея меняется только между записями — иначе отпускание потеряет смысл
                    self.cfg_modified = None;
                    return;
                }
                info!("настройки перечитаны");
                self.adopt(l.config, l.bindings);
            }
            Err(e) => {
                warn!("{e}");
                self.ui.notify(e);
            }
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        self.capture_tick(now);
        if let Some((id, pressed_at)) = self.recording {
            let limit = Duration::from_secs(u64::from(self.cfg.recording.max_seconds));
            if now.duration_since(pressed_at) >= limit {
                info!("диктовка #{id}: лимит длины {} с — останавливаю", self.cfg.recording.max_seconds);
                self.ui.notify(format!(
                    "Запись дошла до лимита {} с — отправляю. Лимит — recording.max_seconds в настройках",
                    self.cfg.recording.max_seconds
                ));
                self.machine.reset();
                self.stop();
                return;
            }
            // сторож удержания: модификаторы отпущены, а отпускания сочетания хук не видел
            if self.machine.holding() && hook::dictate_mods_released() {
                let since = *self.mods_released_since.get_or_insert(now);
                if now.duration_since(since) >= LOST_RELEASE_AFTER {
                    warn!("диктовка #{id}: отпускание сочетания потерялось — останавливаю по модификаторам");
                    self.machine.reset();
                    self.machine.forget_hold();
                    MATCHER.reset_held();
                    self.stop();
                }
            } else {
                self.mods_released_since = None;
            }
        } else if now.duration_since(self.last_cfg_check) >= Duration::from_secs(3) {
            self.last_cfg_check = now;
            self.reload_if_changed();
        }
    }

    fn publish(&self) {
        let status = if self.recording.is_some() {
            Status::Recording
        } else if !self.dictations.is_empty() {
            Status::Transcribing
        } else {
            Status::Ready
        };
        let proxy = self.cfg.proxy.as_ref().map(|p| format!("{}:{}", p.host, p.port));
        let history: Vec<String> = self.history.iter().cloned().collect();
        let engine = self.cfg.engine;
        // ключ OpenRouter нужен, только когда выбран OpenRouter
        let (paused, error, has_key) = (self.paused, self.error.clone(), self.has_key || engine == Engine::Codex);
        let model = match engine {
            Engine::OpenRouter => self.cfg.openrouter.model.clone(),
            Engine::Codex => "вход Codex".to_string(),
        };
        let models = self.cfg.openrouter.models.clone();
        let labels = (self.bindings.pause.label(), self.bindings.dictate.label(), self.bindings.paste_last.label());
        self.ui.update(move |s| {
            s.status = status;
            s.paused = paused;
            s.error = error;
            s.engine = engine.label().into();
            s.codex = engine == Engine::Codex;
            s.model = model;
            s.models = models;
            s.history = history;
            s.pause_label = labels.0;
            s.dictate_label = labels.1;
            s.paste_last_label = labels.2;
            s.has_key = has_key;
            s.proxy = proxy;
        });
        self.push_view();
    }

    /// Окно настроек открыто — шлём ему состояние, если оно изменилось.
    fn push_view(&self) {
        if !self.prefs.open {
            return;
        }
        let v = self.settings_view();
        let mut shown = self.shown_view.borrow_mut();
        if shown.as_ref() != Some(&v) {
            *shown = Some(v.clone());
            settings::push(settings::Msg::View(Box::new(v)));
        }
    }
}

/// Открыть файл программой по умолчанию; связи нет — Блокнотом. В своём потоке: ShellExecute
/// бывает медленным, а контроллер не должен ждать.
fn open_file(path: &Path) {
    let path = path.to_path_buf();
    let _ = std::thread::Builder::new().name("verba-open".into()).spawn(move || {
        use windows::core::{w, PCWSTR};
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let p: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let r = ShellExecuteW(None, w!("open"), PCWSTR(p.as_ptr()), None, None, SW_SHOWNORMAL);
            if r.0 as isize <= 32 {
                let quoted: Vec<u16> =
                    format!("\"{}\"", path.display()).encode_utf16().chain(std::iter::once(0)).collect();
                ShellExecuteW(None, w!("open"), w!("notepad.exe"), PCWSTR(quoted.as_ptr()), None, SW_SHOWNORMAL);
            }
        }
    });
}
