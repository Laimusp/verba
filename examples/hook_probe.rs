//! Пробник: вызывается ли низкоуровневый хук клавиатуры, пока впереди окно Slint этого же процесса.
//! `cargo run --release --example hook_probe [plain]` — `plain`: вместо Slint простое окно Win32.
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::time::Duration;

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

static CALLS: AtomicU32 = AtomicU32::new(0);
static SINK: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// Raw Input клавиатуры процесса — на окно `target` (флаги `flags`).
fn register_raw(target: HWND, flags: windows::Win32::UI::Input::RAWINPUTDEVICE_FLAGS) {
    use windows::Win32::UI::Input::{RegisterRawInputDevices, RAWINPUTDEVICE};
    let dev = RAWINPUTDEVICE { usUsagePage: 0x01, usUsage: 0x06, dwFlags: flags, hwndTarget: target };
    unsafe {
        println!("RegisterRawInputDevices: {:?}", RegisterRawInputDevices(&[dev], std::mem::size_of::<RAWINPUTDEVICE>() as u32));
    }
}

unsafe extern "system" fn hook(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    CALLS.fetch_add(1, Relaxed);
    CallNextHookEx(None, code, w, l)
}

unsafe extern "system" fn plain_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if m == WM_DESTROY {
        PostQuitMessage(0);
    }
    DefWindowProcW(h, m, w, l)
}

slint::slint! {
    export component P inherits Window { width: 300px; height: 200px; title: "hook probe"; Text { text: "probe"; } }
}

fn main() {
    std::thread::spawn(|| unsafe {
        let h = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), Some(GetModuleHandleW(None).unwrap().into()), 0).unwrap();
        let hinst = GetModuleHandleW(None).unwrap();
        let wc = WNDCLASSW { lpfnWndProc: Some(plain_proc), hInstance: hinst.into(), lpszClassName: w!("HookProbeSink"), ..Default::default() };
        RegisterClassW(&wc);
        let sink = CreateWindowExW(WINDOW_EX_STYLE::default(), w!("HookProbeSink"), w!(""), WINDOW_STYLE::default(), 0, 0, 0, 0, Some(HWND_MESSAGE), None, Some(hinst.into()), None).unwrap();
        SINK.store(sink.0 as isize, Relaxed);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
        let _ = UnhookWindowsHookEx(h);
    });
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_secs(1));
        println!("вызовов хука за секунду: {}", CALLS.swap(0, Relaxed));
    });
    let mode = std::env::args().nth(1).unwrap_or_default();
    std::thread::sleep(Duration::from_millis(200));
    if mode == "slint-remove" {
        // снять Raw Input клавиатуры, который зарегистрировал цикл Slint (winit)
        slint::Timer::single_shot(Duration::from_millis(300), || {
            register_raw(HWND::default(), windows::Win32::UI::Input::RIDEV_REMOVE);
        });
    }
    if mode == "slint-sink" {
        // после того как цикл Slint (winit) зарегистрировал свой Raw Input — вернуть клавиатуру себе
        slint::Timer::single_shot(Duration::from_millis(300), || {
            register_raw(HWND(SINK.load(Relaxed) as *mut _), windows::Win32::UI::Input::RIDEV_INPUTSINK);
        });
    }
    if mode.starts_with("plain") {
        unsafe {
            let hinst = GetModuleHandleW(None).unwrap();
            let wc = WNDCLASSW { lpfnWndProc: Some(plain_proc), hInstance: hinst.into(), lpszClassName: w!("HookProbePlain"), ..Default::default() };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("HookProbePlain"),
                w!("hook probe"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                100,
                100,
                300,
                200,
                None,
                None,
                Some(hinst.into()),
                None,
            )
            .unwrap();
            let _ = SetForegroundWindow(hwnd);
            if mode == "plain-raw" {
                register_raw(hwnd, windows::Win32::UI::Input::RIDEV_DEVNOTIFY);
            }
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        return;
    }
    let p = P::new().unwrap();
    p.run().unwrap();
}
