//! Низкоуровневый хук клавиатуры (WH_KEYBOARD_LL) и его сторож.
//!
//! Правило CLAUDE.md §1: обработчик хука только сверяет клавишу с сочетаниями (атомики, без
//! локов и аллокаций), кладёт событие в канал и сразу возвращается — никакого диска, логов, сети.
//! Windows молча снимает хук, если обработчик дольше `LowLevelHooksTimeout`; сторож это видит:
//! Raw Input получает те же нажатия независимо от хука — клавиши идут, а хук молчит > 1,5 с →
//! хук переустанавливается. Ещё переустановка — после разблокировки экрана и выхода из сна.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::OnceLock;
use std::time::Instant;

use crossbeam_channel::Sender;
use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_CONTROL,
    VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::Input::{RegisterRawInputDevices, RAWINPUTDEVICE, RIDEV_INPUTSINK};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostThreadMessageW, RegisterClassW,
    SetTimer, SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION, HHOOK, HWND_MESSAGE, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_INPUT, WM_KEYDOWN, WM_SYSKEYDOWN, WM_TIMER, WNDCLASSW,
};

use super::binding::{modifier_bit, Binding, MOD_ALT, MOD_CTRL, MOD_SHIFT, MOD_WIN};
use crate::config::Bindings;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyId {
    Dictate = 0,
    Cancel = 1,
    Pause = 2,
    PasteLast = 3,
}

const IDS: [HotkeyId; 4] = [HotkeyId::Pause, HotkeyId::Dictate, HotkeyId::PasteLast, HotkeyId::Cancel];

#[derive(Clone, Copy, Debug)]
pub struct HookEvent {
    pub id: HotkeyId,
    pub pressed: bool,
    pub at: Instant,
}

/// Метка своего ввода (Ctrl+V вставки, маска меню): хук пропускает его, не сверяя с сочетаниями.
pub const INJECTED_MARK: usize = 0x5645_5242; // «VERB»
/// Метка своего ввода, который хук глотает: система событие видит (наш процесс «дал последний
/// ввод» → ему можно `SetForegroundWindow`), приложения — нет (`target::win`, приём AutoHotkey).
pub const SWALLOWED_MARK: usize = 0x5645_5243; // «VERC»

/// Состояние сопоставления сочетаний — только атомики: его читает обработчик хука.
pub struct Matcher {
    bindings: [AtomicU32; 4],
    /// нажатие клавиши сочетания проглочено → её отпускание тоже глотаем
    held: [AtomicBool; 4],
    /// основное сочетание «логически нажато»: отпускание ещё не отправлено
    dictate_down: AtomicBool,
    paused: AtomicBool,
    /// Esc перехватывается только во время записи и распознавания
    cancel_armed: AtomicBool,
}

impl Default for Matcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Matcher {
    pub const fn new() -> Self {
        Self {
            bindings: [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)],
            held: [AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false)],
            dictate_down: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            cancel_armed: AtomicBool::new(false),
        }
    }

    pub fn set_bindings(&self, b: &Bindings) {
        self.bindings[HotkeyId::Dictate as usize].store(b.dictate.pack(), Relaxed);
        self.bindings[HotkeyId::Cancel as usize].store(b.cancel.pack(), Relaxed);
        self.bindings[HotkeyId::Pause as usize].store(b.pause.pack(), Relaxed);
        self.bindings[HotkeyId::PasteLast as usize].store(b.paste_last.pack(), Relaxed);
    }

    pub fn binding(&self, id: HotkeyId) -> Option<Binding> {
        Binding::unpack(self.bindings[id as usize].load(Relaxed))
    }

    pub fn set_paused(&self, v: bool) {
        self.paused.store(v, Relaxed);
    }
    pub fn paused(&self) -> bool {
        self.paused.load(Relaxed)
    }
    pub fn set_cancel_armed(&self, v: bool) {
        self.cancel_armed.store(v, Relaxed);
    }

    /// Забыть удержание: отпускание потерялось (экран блокировки, сон). Иначе следующее нажатие
    /// приняли бы за автоповтор и проглотили молча.
    pub fn reset_held(&self) {
        for h in &self.held {
            h.store(false, Relaxed);
        }
        self.dictate_down.store(false, Relaxed);
    }

    /// Решение по одной клавише: глотать ли её и какое событие отправить.
    /// `mods(trigger)` — модификаторы, нажатые сейчас (без класса самой клавиши `trigger`).
    pub fn on_key(&self, vk: u16, down: bool, mods: impl Fn(u16) -> u8) -> (bool, Option<(HotkeyId, bool)>) {
        if !down {
            let mut swallow = false;
            let mut event = None;
            for id in IDS {
                let Some(b) = self.binding(id) else { continue };
                if b.vk == vk && self.held[id as usize].swap(false, Relaxed) {
                    swallow = true;
                    if id == HotkeyId::Dictate && self.dictate_down.swap(false, Relaxed) {
                        event = Some((id, false));
                    }
                }
            }
            if swallow {
                return (true, event);
            }
            // модификатор отпущен раньше клавиши (Ctrl раньше Space) — запись не должна висеть
            if let (Some(bit), Some(b)) = (modifier_bit(vk), self.binding(HotkeyId::Dictate)) {
                if b.mods & bit != 0 && self.dictate_down.swap(false, Relaxed) {
                    return (false, Some((HotkeyId::Dictate, false)));
                }
            }
            return (false, None);
        }
        let paused = self.paused.load(Relaxed);
        for id in IDS {
            let Some(b) = self.binding(id) else { continue };
            if b.vk != vk {
                continue;
            }
            // автоповтор удерживаемого сочетания: глотать молча
            if self.held[id as usize].load(Relaxed) {
                return (true, None);
            }
            // на паузе всё, кроме самой паузы, уходит в приложение нетронутым
            if paused && id != HotkeyId::Pause {
                continue;
            }
            if id == HotkeyId::Cancel {
                // отмена — при любых модификаторах (Esc при зажатом Ctrl+Space), но только во время записи
                if !self.cancel_armed.load(Relaxed) {
                    continue;
                }
            } else if mods(vk) != b.mods {
                continue;
            }
            self.held[id as usize].store(true, Relaxed);
            if id == HotkeyId::Dictate {
                self.dictate_down.store(true, Relaxed);
            }
            return (true, Some((id, true)));
        }
        (false, None)
    }
}

pub static MATCHER: Matcher = Matcher::new();
static TX: OnceLock<Sender<HookEvent>> = OnceLock::new();
static LAST_HOOK_TICK: AtomicU32 = AtomicU32::new(0);
static LAST_RAW_TICK: AtomicU32 = AtomicU32::new(0);
const WM_REINSTALL: u32 = WM_APP + 1;

fn key_down(vk: VIRTUAL_KEY) -> bool {
    unsafe { (GetAsyncKeyState(i32::from(vk.0)) as u16 & 0x8000) != 0 }
}

/// Нажатые сейчас модификаторы (состояние системы), без класса клавиши `trigger`.
pub fn current_mods(trigger: u16) -> u8 {
    let mut m = 0;
    if key_down(VK_CONTROL) {
        m |= MOD_CTRL;
    }
    if key_down(VK_MENU) {
        m |= MOD_ALT;
    }
    if key_down(VK_SHIFT) {
        m |= MOD_SHIFT;
    }
    if key_down(VK_LWIN) || key_down(VK_RWIN) {
        m |= MOD_WIN;
    }
    if let Some(own) = modifier_bit(trigger) {
        m &= !own;
    }
    m
}

/// Когда последний раз шла клавиша (хук или Raw Input), по `GetTickCount`; 0 — хук не запущен.
/// Только клавиатура: движение мыши фокусу не мешает, а нажатия ушли бы не в то окно.
pub fn last_key_tick() -> u32 {
    let (hook, raw) = (LAST_HOOK_TICK.load(Relaxed), LAST_RAW_TICK.load(Relaxed));
    if (raw.wrapping_sub(hook) as i32) > 0 {
        raw
    } else {
        hook
    }
}

/// Хотя бы один модификатор основного сочетания уже отпущен (сторож удержания).
/// Сочетание без модификаторов так не проверить — его клавишу хук глотает, система её не видит.
pub fn dictate_mods_released() -> bool {
    let Some(b) = MATCHER.binding(HotkeyId::Dictate) else { return false };
    if b.mods == 0 {
        return false;
    }
    current_mods(b.vk) & b.mods != b.mods
}

/// Маска меню: после проглоченного сочетания с Alt или Win отпускание Alt открыло бы меню окна,
/// а Win — меню «Пуск». Пустое нажатие неназначенной клавиши между ними это гасит (как AutoHotkey).
pub fn inject_menu_mask() {
    const VK_UNASSIGNED: u16 = 0xE8;
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(VK_UNASSIGNED), wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: INJECTED_MARK },
        },
    };
    let inputs = [key(Default::default()), key(KEYEVENTF_KEYUP)];
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        LAST_HOOK_TICK.store(GetTickCount(), Relaxed);
        if kb.dwExtraInfo == SWALLOWED_MARK {
            return LRESULT(1);
        }
        if kb.dwExtraInfo != INJECTED_MARK {
            let msg = wparam.0 as u32;
            let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let (swallow, event) = MATCHER.on_key(kb.vkCode as u16, down, current_mods);
            if let (Some((id, pressed)), Some(tx)) = (event, TX.get()) {
                // неограниченный канал: send не ждёт никогда
                let _ = tx.send(HookEvent { id, pressed, at: Instant::now() });
            }
            if swallow {
                return LRESULT(1);
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

thread_local! {
    static HOOK: Cell<Option<HHOOK>> = const { Cell::new(None) };
    static NOTED: Cell<(u32, u32)> = const { Cell::new((0, 0)) };
}

fn install() -> Result<(), String> {
    HOOK.with(|h| {
        if let Some(old) = h.take() {
            unsafe {
                let _ = UnhookWindowsHookEx(old);
            }
        }
        let hinst = unsafe { GetModuleHandleW(None) }.map_err(|e| e.to_string())?;
        let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), Some(HINSTANCE(hinst.0)), 0) }
            .map_err(|e| format!("хук клавиатуры не встал: {e}"))?;
        h.set(Some(hook));
        LAST_HOOK_TICK.store(unsafe { GetTickCount() }, Relaxed);
        Ok(())
    })
}

fn reinstall(why: &str) {
    match install() {
        Ok(()) => {
            // одна строка в минуту: в окне с правами администратора (UIPI) сторож может срабатывать часто
            let now = unsafe { GetTickCount() };
            let (last, skipped) = NOTED.with(|n| n.get());
            if last == 0 || now.wrapping_sub(last) > 60_000 {
                let more = if skipped > 0 { format!(" (ещё {skipped} раз за минуту)") } else { String::new() };
                crate::info!("хук клавиатуры переустановлен: {why}{more}");
                NOTED.with(|n| n.set((now, 0)));
            } else {
                NOTED.with(|n| n.set((last, skipped + 1)));
            }
        }
        Err(e) => crate::error!("{e}"),
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_INPUT => {
            LAST_RAW_TICK.store(GetTickCount(), Relaxed);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_TIMER => {
            let now = GetTickCount();
            let raw = LAST_RAW_TICK.load(Relaxed);
            let hook = LAST_HOOK_TICK.load(Relaxed);
            // клавиши шли недавно, а хук не видел их дольше 1,5 с — Windows его сняла
            if raw != 0 && now.wrapping_sub(raw) < 10_000 && (raw.wrapping_sub(hook) as i32) > 1500 {
                reinstall(&format!("клавиши шли, а хук молчал {} мс", raw.wrapping_sub(hook)));
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Поток хука: свой цикл сообщений (без него обработчик хука не вызывается).
pub struct HookThread {
    thread_id: u32,
}

impl HookThread {
    pub fn spawn(tx: Sender<HookEvent>) -> Result<HookThread, String> {
        let _ = TX.set(tx);
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<u32, String>>(1);
        std::thread::Builder::new()
            .name("verba-hook".into())
            .spawn(move || unsafe { run(ready_tx) })
            .map_err(|e| e.to_string())?;
        let thread_id = ready_rx.recv().map_err(|_| "поток хука не стартовал".to_string())??;
        Ok(HookThread { thread_id })
    }

    /// Переустановить хук (после разблокировки экрана, сна).
    pub fn reinstall(&self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_REINSTALL, WPARAM(0), LPARAM(0));
        }
    }
}

unsafe fn run(ready: Sender<Result<u32, String>>) {
    let thread_id = GetCurrentThreadId();
    let hinst = match GetModuleHandleW(None) {
        Ok(h) => HINSTANCE(h.0),
        Err(e) => {
            let _ = ready.send(Err(e.to_string()));
            return;
        }
    };
    let class = w!("VerbaHookWatch");
    let wc = WNDCLASSW { lpfnWndProc: Some(wnd_proc), hInstance: hinst, lpszClassName: class, ..Default::default() };
    RegisterClassW(&wc);
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        class,
        w!(""),
        WINDOW_STYLE::default(),
        0,
        0,
        0,
        0,
        Some(HWND_MESSAGE),
        None,
        Some(hinst),
        None,
    );
    match &hwnd {
        Ok(hwnd) => {
            // Raw Input приходит и без хука: по нему сторож видит, что клавиши идут
            let dev = RAWINPUTDEVICE { usUsagePage: 0x01, usUsage: 0x06, dwFlags: RIDEV_INPUTSINK, hwndTarget: *hwnd };
            if let Err(e) = RegisterRawInputDevices(&[dev], std::mem::size_of::<RAWINPUTDEVICE>() as u32) {
                crate::warn!("сторож хука без Raw Input ({e}): снятый хук переустановится только после разблокировки и сна");
            }
            SetTimer(Some(*hwnd), 1, 1000, None);
        }
        Err(e) => crate::warn!("окно сторожа хука не создано: {e}"),
    }
    if let Err(e) = install() {
        let _ = ready.send(Err(e));
        return;
    }
    let _ = ready.send(Ok(thread_id));
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        if msg.hwnd.is_invalid() && msg.message == WM_REINSTALL {
            reinstall("разблокировка экрана или выход из сна");
            continue;
        }
        DispatchMessageW(&msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher() -> Matcher {
        let m = Matcher::new();
        let mut c = crate::config::Config::default();
        m.set_bindings(&c.validate().unwrap());
        m
    }

    const SPACE: u16 = 0x20;
    const CTRL: u16 = 0xA2;
    const ESC: u16 = 0x1B;
    const P: u16 = 0x50;

    #[test]
    fn ctrl_space_is_swallowed_with_press_and_release() {
        let m = matcher();
        assert_eq!(m.on_key(SPACE, true, |_| MOD_CTRL), (true, Some((HotkeyId::Dictate, true))));
        assert_eq!(m.on_key(SPACE, true, |_| MOD_CTRL), (true, None), "автоповтор");
        assert_eq!(m.on_key(SPACE, false, |_| MOD_CTRL), (true, Some((HotkeyId::Dictate, false))));
        assert_eq!(m.on_key(CTRL, false, |_| 0), (false, None));
    }

    #[test]
    fn plain_space_and_wrong_mods_pass() {
        let m = matcher();
        assert_eq!(m.on_key(SPACE, true, |_| 0), (false, None));
        assert_eq!(m.on_key(SPACE, true, |_| MOD_CTRL | MOD_SHIFT), (false, None));
        assert_eq!(m.on_key(SPACE, false, |_| 0), (false, None));
    }

    #[test]
    fn ctrl_released_first_ends_dictation_once() {
        let m = matcher();
        m.on_key(SPACE, true, |_| MOD_CTRL);
        assert_eq!(m.on_key(CTRL, false, |_| 0), (false, Some((HotkeyId::Dictate, false))), "Ctrl отпущен — запись не висит");
        assert_eq!(m.on_key(SPACE, false, |_| 0), (true, None), "отпускание Space глотаем, второго события нет");
    }

    #[test]
    fn paused_lets_dictate_through_but_not_pause() {
        let m = matcher();
        m.set_paused(true);
        assert_eq!(m.on_key(SPACE, true, |_| MOD_CTRL), (false, None), "на паузе Ctrl+Space уходит в приложение");
        assert_eq!(m.on_key(SPACE, false, |_| MOD_CTRL), (false, None));
        assert_eq!(m.on_key(P, true, |_| MOD_CTRL | MOD_ALT), (true, Some((HotkeyId::Pause, true))));
        assert_eq!(m.on_key(P, false, |_| MOD_CTRL | MOD_ALT), (true, None));
    }

    #[test]
    fn esc_only_while_armed_and_with_any_mods() {
        let m = matcher();
        assert_eq!(m.on_key(ESC, true, |_| 0), (false, None));
        m.set_cancel_armed(true);
        assert_eq!(m.on_key(ESC, true, |_| MOD_CTRL), (true, Some((HotkeyId::Cancel, true))));
        assert_eq!(m.on_key(ESC, false, |_| MOD_CTRL), (true, None));
    }
}
