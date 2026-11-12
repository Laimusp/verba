//! Вставка в запомненное поле (`docs/focus-restore.md`).
//!
//! При нажатии хоткея — снимок: переднее окно и окно-поле в фокусе (Win32, сразу, десятки
//! микросекунд, чужой процесс не ждёт) и элемент UI Automation (поток снимка: первый запрос к
//! Chrome/Electron включает их дерево доступности и бывает медленным, контроллер его не ждёт).
//!
//! После распознавания поток доставки выбирает способ:
//! 1. фокус на том же поле → обычная вставка (`paste::paste`);
//! 2. классическое поле Edit/RichEdit → `EM_REPLACESEL` в его каретку, фокус не трогается;
//! 3. игра (полноэкранное окно или exe из списка) — фокус не уводится и не возвращается: Ctrl+V в
//!    игру с закрытым чатом стал бы нажатиями игровых клавиш → не вставлять;
//! 4. иначе → пауза во вводе человека → буфер готовится → окно вперёд → фокус на поле → Ctrl+V →
//!    первое чтение буфера и отпущенный Ctrl+V → фокус обратно туда, где был человек;
//! 5. окна или поля нет → не вставлять никуда; текст ждёт «вставить последнее».
//!
//! Ctrl+V уходит, только когда проверено, что фокус на запомненном поле: иначе текст попал бы
//! в чужое поле. Доставки идут по одной: фокус и буфер обмена у системы одни.

pub mod uia;
pub mod win;

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use windows::Win32::UI::Accessibility::IUIAutomationElement;

use crate::config::PasteMethod;
use crate::paste::{self, Armed, Milestone, PasteReport};
use crate::{info, warn};
use uia::{Field, Uia};
use win::{Front, ReplaceError, Window};

/// Снимок UIA старше этого не берётся: человек мог уже уйти из поля.
const STALE_SNAPSHOT: Duration = Duration::from_millis(1500);
/// Доставка ждёт запоздавший снимок UIA не дольше.
const UIA_SNAPSHOT_WAIT: Duration = Duration::from_millis(500);
/// UIA отдала окно вместо поля — переспросить столько раз через такую паузу.
const WINDOW_LEVEL_RETRIES: u32 = 6;
const WINDOW_LEVEL_PAUSE: Duration = Duration::from_millis(50);
/// Пауза во вводе человека перед перескоком и сколько её ждать.
const INPUT_QUIET: Duration = Duration::from_millis(200);
const INPUT_PAUSE_MAX: Duration = Duration::from_secs(3);
/// Буфер на взводе готовится не дольше (снимок буфера, публикация).
const READY_WAIT: Duration = Duration::from_secs(2);
/// Фокус на поле после активации окна и SetFocus — ждать не дольше.
const FOCUS_WAIT: Duration = Duration::from_millis(250);
/// Приложение само возвращает фокус на своё поле при активации окна — подождать его столько.
const SELF_FOCUS_WAIT: Duration = Duration::from_millis(60);
/// После Ctrl+V ждём первое чтение буфера не дольше — дальше фокус возвращается человеку.
const READ_WAIT: Duration = Duration::from_millis(700);
/// Отпущенные клавиши доходят до окна-цели за столько, прежде чем фокус уйдёт обратно.
const KEYUP_SETTLE: Duration = Duration::from_millis(8);

/// Поле, где был курсор при нажатии хоткея.
pub struct Snapshot {
    pub at: Instant,
    /// None — переднего окна не было (переключение окон, экран блокировки)
    pub window: Option<Window>,
    /// сколько занял Win32-снимок, мкс
    pub capture_us: u64,
    uia: Receiver<Option<Field>>,
}

/// Почему текст не вставлен.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    WindowGone,
    FieldGone,
    /// запомненное окно — игра, а фокус ушёл из неё
    Game,
    /// впереди сейчас игра или полноэкранное окно — фокус у него не забираем
    GameInFront,
    /// человек печатал без паузы
    Busy,
    FrontRefused,
    NotFocused,
    ReadOnly,
    /// окно вернули вперёд, а Ctrl+V приложение не приняло
    NotRead,
}

impl Why {
    /// «что не так → что сделать»; `paste_last` — сочетание «вставить последнее».
    pub fn words(self, paste_last: &str) -> String {
        let again = format!("«вставить последнее» ({paste_last}) вставит текст туда, где курсор сейчас");
        match self {
            Why::WindowGone => format!("Окно, где начал диктовку, закрыто — текст не вставил → {again}"),
            Why::FieldGone => format!("Поле, где был курсор, пропало — текст не вставил → {again}"),
            Why::Game => format!(
                "Ты ушёл из игры — текст не вставил, чтобы не нажать игровые клавиши → открой чат игры и нажми «вставить последнее» ({paste_last})"
            ),
            Why::GameInFront => format!("Впереди полноэкранное окно — фокус у него не забирал, текст не вставил → {again}"),
            Why::Busy => format!("Ты печатал без паузы — фокус не забирал, текст не вставил → {again}"),
            Why::FrontRefused => format!("Windows не дала вернуть окно вперёд — текст не вставил → {again}"),
            Why::NotFocused => format!("Не смог вернуть курсор в прежнее поле — текст не вставил → {again}"),
            Why::ReadOnly => format!("Поле только для чтения — текст не вставил → {again}"),
            Why::NotRead => format!("Окно не приняло вставку → {again}"),
        }
    }
}

/// Как доставлен текст.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    /// фокус на том же поле — обычная вставка; итог вставки придёт отдельно (`PasteReport`)
    Same,
    /// `EM_REPLACESEL` — вставлено, фокус не тронут
    ReplaceSel,
    /// окно возвращено вперёд, Ctrl+V, фокус возвращён; итог вставки придёт отдельно
    Restored { read: bool },
    NotPasted(Why),
    /// вставка не началась (буфер занят и т. п.) — ошибка придёт итогом вставки
    PasteError,
}

/// Итог доставки для контроллера: способ и строка для лога (без текста диктовки и имён полей).
#[derive(Debug, Clone)]
pub struct Delivered {
    pub id: u64,
    pub how: How,
    pub detail: String,
    /// от получения задания до Ctrl+V (или до вставки `EM_REPLACESEL`), мс
    pub ms: u64,
}

pub struct Job {
    pub id: u64,
    pub text: String,
    pub snapshot: Arc<Snapshot>,
    pub method: PasteMethod,
    pub restore_clipboard: bool,
    pub games: Vec<String>,
    pub done: Box<dyn FnOnce(PasteReport) + Send>,
}

struct SnapJob {
    at: Instant,
    top: win::Hwnd,
    reply: Sender<Option<Field>>,
}

/// Игра: полноэкранное окно или exe из списка (`paste.games`).
pub fn is_game(w: &Window, games: &[String]) -> bool {
    w.fullscreen || games.iter().any(|g| g == &w.exe)
}

pub struct TargetService {
    snap_tx: Sender<SnapJob>,
    job_tx: Sender<Job>,
}

impl TargetService {
    pub fn spawn(events: Sender<Delivered>) -> Result<TargetService, String> {
        let (snap_tx, snap_rx) = unbounded::<SnapJob>();
        let (job_tx, job_rx) = unbounded::<Job>();
        std::thread::Builder::new()
            .name("verba-snapshot".into())
            .spawn(move || snapshot_loop(snap_rx))
            .map_err(|e| format!("поток снимка поля: {e}"))?;
        std::thread::Builder::new()
            .name("verba-deliver".into())
            .spawn(move || deliver_loop(job_rx, events))
            .map_err(|e| format!("поток доставки: {e}"))?;
        Ok(TargetService { snap_tx, job_tx })
    }

    /// Снимок поля сейчас (при нажатии хоткея). Не ждёт ни UIA, ни чужой процесс.
    pub fn capture(&self, games: &[String]) -> Arc<Snapshot> {
        let at = Instant::now();
        let window = win::foreground();
        let capture_us = at.elapsed().as_micros() as u64;
        let (reply, uia) = bounded(1);
        match &window {
            // у игры полей для Windows нет — UIA не спрашиваем
            Some(w) if !is_game(w, games) => {
                let _ = self.snap_tx.send(SnapJob { at, top: w.top, reply });
            }
            _ => {
                let _ = reply.send(None);
            }
        }
        Arc::new(Snapshot { at, window, capture_us, uia })
    }

    /// Доставить текст в поле снимка. Итог — событием `Delivered`, итог Ctrl+V — в `job.done`.
    pub fn deliver(&self, job: Job) {
        if let Err(e) = self.job_tx.send(job) {
            // поток доставки умер — вставка в текущий фокус, как без переключателя
            let job = e.0;
            warn!("вставка #{}: поток доставки не работает — вставляю в текущий фокус", job.id);
            paste::paste(job.text, job.method, job.restore_clipboard, job.done);
        }
    }
}

fn snapshot_loop(rx: Receiver<SnapJob>) {
    let uia = match Uia::new() {
        Ok(u) => Some(u),
        Err(e) => {
            warn!("UI Automation недоступна ({e}) — поле запоминается только по окну");
            None
        }
    };
    for job in rx {
        let field = uia.as_ref().and_then(|u| snap_field(u, &job));
        let _ = job.reply.send(field);
    }
}

fn snap_field(u: &Uia, job: &SnapJob) -> Option<Field> {
    if job.at.elapsed() > STALE_SNAPSHOT {
        return None;
    }
    let t = Instant::now();
    let mut tries = 0;
    loop {
        let el = u.focused().ok()?;
        // фокус мог уйти, пока шёл запрос: элемент — того окна, только если оно всё ещё впереди
        if win::foreground_hwnd() != job.top {
            return None;
        }
        let field = u.describe(el, t.elapsed().as_millis() as u64).ok()?;
        // Chrome/Electron на первый запрос отдают окно: дерево доступности включается — спросить ещё
        if !field.is_window_level() || tries >= WINDOW_LEVEL_RETRIES || job.at.elapsed() > STALE_SNAPSHOT {
            return Some(field);
        }
        tries += 1;
        std::thread::sleep(WINDOW_LEVEL_PAUSE);
    }
}

fn deliver_loop(rx: Receiver<Job>, events: Sender<Delivered>) {
    win::ensure_message_queue();
    let uia = match Uia::new() {
        Ok(u) => Some(u),
        Err(e) => {
            warn!("UI Automation недоступна ({e}) — фокус возвращается только по окну");
            None
        }
    };
    for job in rx {
        Delivery { uia: uia.as_ref(), events: &events, id: job.id, started: Instant::now() }.run(job);
    }
}

struct Delivery<'a> {
    uia: Option<&'a Uia>,
    events: &'a Sender<Delivered>,
    id: u64,
    started: Instant,
}

/// Где был человек перед перескоком — туда фокус и вернётся.
struct Person {
    window: Option<Window>,
    /// элемент в фокусе, если человек в том же окне, что и запомненное поле
    element: Option<IUIAutomationElement>,
}

impl Delivery<'_> {
    fn send(&self, how: How, detail: String) {
        let ms = self.started.elapsed().as_millis() as u64;
        let _ = self.events.send(Delivered { id: self.id, how, detail, ms });
    }

    fn run(&self, job: Job) {
        let Job { text, snapshot, method, restore_clipboard, games, done, .. } = job;
        let Some(target) = snapshot.window.clone() else {
            self.send(How::Same, "переднего окна при нажатии не было — в текущий фокус".into());
            return paste::paste(text, method, restore_clipboard, done);
        };
        let field = snapshot.uia.recv_timeout(UIA_SNAPSHOT_WAIT).ok().flatten();
        let what = match &field {
            Some(f) => format!("{} [{} · {}, UIA {} мс]", target.exe, target.class, f.label(), f.ms),
            None => format!("{} [{}, без UIA]", target.exe, target.class),
        };
        // UIA так и не отдала поле, только окно: поле — то, что окно само вернёт при активации
        let field = field.filter(|f| !f.is_window_level());
        if !win::is_window(target.top) {
            return self.send(How::NotPasted(Why::WindowGone), format!("{what}: окно закрыто"));
        }
        let now = win::foreground();
        let game = is_game(&target, &games);
        if let Some(now) = &now {
            // в игре полей для Windows нет: окно игры впереди — значит, фокус на месте
            if now.top == target.top && (game || self.same_field(field.as_ref(), &target, now)) {
                self.send(How::Same, format!("{what}: то же поле"));
                return paste::paste(text, method, restore_clipboard, done);
            }
        }
        if win::is_classic_edit(&target.class) && win::is_window(target.focus) {
            let t = Instant::now();
            match win::replace_sel(target.focus, &text) {
                Ok(()) => {
                    let ms = t.elapsed().as_millis();
                    return self.send(How::ReplaceSel, format!("{what}: EM_REPLACESEL за {ms} мс, фокус не тронут"));
                }
                Err(ReplaceError::Gone) => return self.send(How::NotPasted(Why::FieldGone), format!("{what}: поле закрыто")),
                Err(ReplaceError::ReadOnly) => {
                    return self.send(How::NotPasted(Why::ReadOnly), format!("{what}: поле только для чтения"))
                }
                Err(e) => warn!("вставка #{}: EM_REPLACESEL не сработал ({e:?}) — возвращаю фокус", self.id),
            }
        }
        if game {
            return self.send(How::NotPasted(Why::Game), format!("{what}: игра, фокус ушёл"));
        }
        if let Some(now) = now.as_ref().filter(|n| is_game(n, &games)) {
            return self.send(How::NotPasted(Why::GameInFront), format!("{what}: впереди игра {}", now.exe));
        }
        self.restore(text, method, restore_clipboard, &target, field, &what, done);
    }

    /// Фокус сейчас на запомненном поле: по UIA, если снимок есть, иначе по окну-полю.
    fn same_field(&self, field: Option<&Field>, target: &Window, now: &Window) -> bool {
        if let (Some(u), Some(f)) = (self.uia, field) {
            if let Ok(cur) = u.focused() {
                return u.same(&cur, &f.element.0);
            }
        }
        now.focus == target.focus
    }

    #[allow(clippy::too_many_arguments)]
    fn restore(
        &self,
        text: String,
        method: PasteMethod,
        restore_clipboard: bool,
        target: &Window,
        field: Option<Field>,
        what: &str,
        done: Box<dyn FnOnce(PasteReport) + Send>,
    ) {
        if !win::wait_input_pause(INPUT_QUIET, INPUT_PAUSE_MAX) {
            return self.send(How::NotPasted(Why::Busy), format!("{what}: ввод без паузы {} с", INPUT_PAUSE_MAX.as_secs()));
        }
        let person = self.person(target);
        // буфер готовится до перескока: окно впереди только на время Ctrl+V
        let (tx, rx) = unbounded();
        paste::paste_armed(text, method, restore_clipboard, tx, move |r: PasteReport| {
            // отменённая нами вставка — не событие для человека: о ней скажет доставка
            if !r.aborted {
                done(r);
            }
        });
        let armed = loop {
            match rx.recv_timeout(READY_WAIT) {
                Ok(Milestone::Ready(h)) => break Armed::new(h),
                Ok(Milestone::Ended) | Err(_) => {
                    return self.send(How::PasteError, format!("{what}: буфер не подготовился"));
                }
                Ok(_) => {}
            }
        };
        let was_iconic = win::is_iconic(target.top);
        let t0 = Instant::now();
        let Some(front) = win::bring_to_front(target.top) else {
            armed.cancel();
            self.give_back(&person, target, was_iconic);
            let why = if win::is_window(target.top) { Why::FrontRefused } else { Why::WindowGone };
            return self.send(How::NotPasted(why), format!("{what}: окно не встало вперёд"));
        };
        let front_ms = t0.elapsed().as_millis();
        let focused = match self.focus_field(field.as_ref(), target) {
            Ok(how) => how,
            Err(why) => {
                armed.cancel();
                self.give_back(&person, target, was_iconic);
                let ms = t0.elapsed().as_millis();
                return self.send(How::NotPasted(why), format!("{what}: фокус на поле не встал ({why:?}), перескок {ms} мс"));
            }
        };
        let fired_ms = t0.elapsed().as_millis();
        armed.fire();
        let (mut sent, mut read) = (false, false);
        let deadline = Instant::now() + READ_WAIT;
        while !(sent && read) {
            match rx.recv_deadline(deadline) {
                Ok(Milestone::ChordSent) => sent = true,
                Ok(Milestone::FirstRead) => read = true,
                Ok(Milestone::ChordFailed) | Ok(Milestone::Ended) | Err(_) => break,
                Ok(Milestone::Ready(_)) => {}
            }
        }
        if sent {
            // отпускание Ctrl+V система раздаёт асинхронно: пусть дойдёт до окна-цели, а не до человека
            std::thread::sleep(KEYUP_SETTLE);
        }
        self.give_back(&person, target, was_iconic);
        let jump = t0.elapsed().as_millis();
        let mut detail = format!(
            "{what}: окно вперёд за {front_ms} мс ({}), фокус — {focused}, Ctrl+V через {fired_ms} мс, перескок {jump} мс",
            front.words()
        );
        if !read {
            detail.push_str(", буфер не прочитан");
        }
        if front == Front::Already {
            info!("вставка #{}: запомненное окно уже было впереди, поле — другое", self.id);
        }
        self.send(How::Restored { read }, detail);
    }

    fn person(&self, target: &Window) -> Person {
        let window = win::foreground();
        let element =
            if window.as_ref().is_some_and(|w| w.top == target.top) { self.uia.and_then(|u| u.focused().ok()) } else { None };
        Person { window, element }
    }

    /// Фокус на запомненное поле с проверкой: без неё Ctrl+V ушёл бы в чужое поле.
    fn focus_field(&self, field: Option<&Field>, target: &Window) -> Result<&'static str, Why> {
        if let (Some(u), Some(f)) = (self.uia, field) {
            // активированное окно обычно само возвращает фокус на своё поле (Chrome, Qt)
            if poll(SELF_FOCUS_WAIT, || u.has_focus(&f.element.0)) {
                return Ok("приложение вернуло само");
            }
            let (el, how) = match unsafe { f.element.0.SetFocus() } {
                Ok(()) => (f.element.0.clone(), "UIA SetFocus"),
                Err(e) if uia::is_gone(&e) => {
                    // RuntimeId протух: страница пересоздала поле — ищем его в том же окне
                    let Some(found) = u.refind(win::hwnd(target.top), f) else { return Err(Why::FieldGone) };
                    if unsafe { found.SetFocus() }.is_err() {
                        return Err(Why::NotFocused);
                    }
                    (found, "UIA SetFocus найденного заново")
                }
                Err(_) => return Err(Why::NotFocused),
            };
            return if poll(FOCUS_WAIT, || u.has_focus(&el)) { Ok(how) } else { Err(Why::NotFocused) };
        }
        // без UIA — по окну-полю: окно само вернуло фокус или ставим его через AttachThreadInput
        if poll(SELF_FOCUS_WAIT, || win::focus_of(target.thread).0 == target.focus) {
            return Ok("окно вернуло само");
        }
        if win::focus_hwnd(target.focus, target.thread) {
            return Ok("SetFocus");
        }
        Err(if win::is_window(target.focus) { Why::NotFocused } else { Why::FieldGone })
    }

    /// Вернуть фокус человеку: его окно вперёд (наш Ctrl+V только что сделал нас «последним
    /// вводом» — Windows это разрешит), а в том же окне — его элемент.
    fn give_back(&self, person: &Person, target: &Window, was_iconic: bool) {
        if let Some(p) = &person.window {
            if p.top != target.top {
                if win::bring_to_front(p.top).is_none() {
                    warn!("вставка #{}: фокус человеку не вернул — окно {} не встало вперёд", self.id, p.exe);
                }
            } else if let Some(el) = &person.element {
                let _ = unsafe { el.SetFocus() };
            } else if p.focus != target.focus {
                win::focus_hwnd(p.focus, p.thread);
            }
        }
        if was_iconic {
            win::minimize_quietly(target.top);
        }
    }
}

fn poll(max: Duration, mut ok: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + max;
    loop {
        if ok() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn games_by_exe_or_fullscreen() {
        let mut w = Window { exe: "cs2.exe".into(), ..Default::default() };
        assert!(is_game(&w, &["cs2.exe".into()]));
        assert!(!is_game(&w, &[]));
        w.fullscreen = true;
        assert!(is_game(&w, &[]));
    }

    #[test]
    fn words_name_the_fix() {
        for why in [Why::WindowGone, Why::FieldGone, Why::Game, Why::GameInFront, Why::Busy, Why::FrontRefused, Why::NotFocused, Why::ReadOnly, Why::NotRead] {
            let w = why.words("Ctrl+Alt+V");
            assert!(w.contains("→") && w.contains("Ctrl+Alt+V"), "{why:?}: {w}");
        }
    }
}
