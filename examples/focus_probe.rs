//! Живой замер вставки в запомненное поле (`docs/focus-restore.md`).
//!
//! Раунд: окно-цель вперёд, курсор в поле → снимок (как при нажатии хоткея) → «человек» уходит в
//! своё окно (форма PowerShell с полем) → доставка текста → проверка: текст в поле цели, фокус
//! вернулся человеку, в его поле ничего не попало. Перескок меряется снаружи: отдельный поток каждую
//! миллисекунду смотрит, сколько окно-цель было впереди.
//!
//! ```text
//! cargo run --release --example focus_probe -- <сценарий> [раундов]
//! ```
//! Сценарии: `notepad`, `chrome-input`, `chrome-textarea`, `chrome-editable`, `chrome-recreate`,
//! `chrome-remove`, `vscode`, `winforms`, `game-fullscreen`, `game-list`, `closed`, `same`, `off`.
//! Окна открывает сам (Chrome и VS Code — с временным профилем) и закрывает в конце.
//!
//! `e2e-chrome`, `e2e-winforms` — сквозной прогон через сам Verba (`target/release/verba.exe`,
//! `VERBA_HOME` во временной папке): хоткей Ctrl+Alt+F12 через SendInput → запись с микрофона → фейковый
//! сервер распознавания на 127.0.0.1 (ключ и звук дальше машины не уходят) → уход в окно человека →
//! текст `e2e-N` должен встать в поле цели. Перед запуском Verba владельца закрыть (один экземпляр).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::unbounded;
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_CONTROL, VK_F12, VK_MENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, PostMessageW, SendMessageTimeoutW,
    SMTO_ABORTIFHUNG, WM_CLOSE, WM_GETTEXT,
};

use verba::config::PasteMethod;
use verba::paste::{self, PasteReport};
use verba::target::win::{self, Hwnd};
use verba::target::{Delivered, How, Job, TargetService};

const PS_FORM: &str = r#"
param([string]$Title, [switch]$Full, [switch]$NoBox)
Add-Type -AssemblyName System.Windows.Forms
$f = New-Object Windows.Forms.Form
$f.Text = $Title
$f.Width = 520; $f.Height = 160
$f.StartPosition = 'Manual'; $f.Left = 60; $f.Top = 60
if ($Full) { $f.FormBorderStyle = 'None'; $f.WindowState = 'Maximized' }
$f.KeyPreview = $true
$f.Add_KeyDown({ param($s, $e) if ($e.Control -and $e.KeyCode -eq 'V' -and $NoBox) { $f.Text = "$Title|" + [Windows.Forms.Clipboard]::GetText() } })
if (-not $NoBox) {
  $b = New-Object Windows.Forms.TextBox
  $b.Width = 480; $b.Left = 10; $b.Top = 10
  $f.Controls.Add($b)
  $b.Add_TextChanged({ $f.Text = "$Title|" + $b.Text })
}
[Windows.Forms.Application]::Run($f)
"#;

const PAGE: &str = r#"<!doctype html><meta charset=utf-8><title>probe|</title>
<body style="font:16px sans-serif">
<input id=i style="width:500px"><br><br><textarea id=t style="width:500px"></textarea><br><br>
<div id=c contenteditable style="width:500px;min-height:40px;background:#eee"></div>
<script>
const [fid, act] = (location.hash.slice(1) || 'i').split('-');
const field = () => document.getElementById(fid);
const show = () => { const f = field(); document.title = 'probe|' + (f ? (f.value ?? f.textContent) : '<нет поля>'); };
document.addEventListener('input', show);
field().focus();
// как чаты: окно получило фокус, а курсор нигде — курсор в поле
window.addEventListener('focus', () => { if (act === 'recreate' && document.activeElement === document.body) field().focus(); });
window.addEventListener('blur', () => {
  // перерисовка: поле пересоздаётся (старый RuntimeId протухает) или убирается
  if (act === 'recreate') setTimeout(() => field().replaceWith(field().cloneNode(true)), 50);
  if (act === 'remove') setTimeout(() => { field().remove(); show(); }, 50);
});
show();
</script>"#;

fn title(h: Hwnd) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(win::hwnd(h), &mut buf) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..n])
}

/// Видимое верхнее окно, чей заголовок начинается с `prefix` (и процесс `pid`, если задан).
fn find_window(prefix: &str, pid: Option<u32>, wait: Duration) -> Option<Hwnd> {
    struct Ctx {
        prefix: String,
        pid: Option<u32>,
        found: Hwnd,
    }
    unsafe extern "system" fn each(h: HWND, l: LPARAM) -> BOOL {
        let ctx = &mut *(l.0 as *mut Ctx);
        if IsWindowVisible(h).as_bool() {
            let raw = h.0 as isize;
            let mut p = 0;
            GetWindowThreadProcessId(h, Some(&mut p));
            if title(raw).starts_with(&ctx.prefix) && ctx.pid.is_none_or(|x| x == p) {
                ctx.found = raw;
                return BOOL(0);
            }
        }
        BOOL(1)
    }
    let deadline = Instant::now() + wait;
    loop {
        let mut ctx = Ctx { prefix: prefix.to_string(), pid, found: 0 };
        unsafe {
            let _ = EnumWindows(Some(each), LPARAM(&mut ctx as *mut Ctx as isize));
        }
        if ctx.found != 0 {
            return Some(ctx.found);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Ни клавиатуры, ни мыши `need` (наши SendInput тоже считаются — после раунда ждём заново).
fn wait_user_idle(need: Duration, max: Duration) -> bool {
    let deadline = Instant::now() + max;
    while Instant::now() < deadline {
        if win::keys_idle_ms() >= need.as_millis() as u32 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn chord(keys: &[u16]) {
    let ev = |vk: u16, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() }, ..Default::default() },
        },
    };
    let mut seq: Vec<INPUT> = keys.iter().map(|k| ev(*k, false)).collect();
    seq.extend(keys.iter().rev().map(|k| ev(*k, true)));
    unsafe {
        SendInput(&seq, std::mem::size_of::<INPUT>() as i32);
    }
}

fn edit_text(field: Hwnd) -> String {
    let mut buf = vec![0u16; 64 * 1024];
    let mut out = 0usize;
    unsafe {
        SendMessageTimeoutW(
            win::hwnd(field),
            WM_GETTEXT,
            WPARAM(buf.len()),
            LPARAM(buf.as_mut_ptr() as isize),
            SMTO_ABORTIFHUNG,
            1000,
            Some(&mut out),
        );
    }
    String::from_utf16_lossy(&buf[..out.min(buf.len())])
}

fn close(h: Hwnd) {
    unsafe {
        let _ = PostMessageW(Some(win::hwnd(h)), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

struct Form {
    child: Child,
    top: Hwnd,
    title: String,
}

impl Form {
    fn open(dir: &Path, title: &str, args: &[&str]) -> Form {
        let script = dir.join("form.ps1");
        std::fs::write(&script, PS_FORM).expect("form.ps1");
        let child = Command::new("powershell.exe")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(&script)
            .args(["-Title", title])
            .args(args)
            .spawn()
            .expect("powershell");
        let top = find_window(title, Some(child.id()), Duration::from_secs(20)).expect("окно формы не появилось");
        Form { child, top, title: title.into() }
    }
    fn text(&self) -> String {
        title(self.top).strip_prefix(&format!("{}|", self.title)).unwrap_or("").to_string()
    }
}

impl Drop for Form {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// Сколько окно было впереди, пока шла доставка (замер снаружи), и сколько раз вставало.
struct Watch {
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<(u64, u32)>,
}

impl Watch {
    fn start(target: Hwnd) -> Watch {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut front_us = 0u64;
            let mut jumps = 0u32;
            let mut since: Option<Instant> = None;
            while !s.load(Ordering::Relaxed) {
                let now_front = win::foreground_hwnd() == target;
                match (now_front, since) {
                    (true, None) => {
                        since = Some(Instant::now());
                        jumps += 1;
                    }
                    (false, Some(t)) => {
                        front_us += t.elapsed().as_micros() as u64;
                        since = None;
                    }
                    _ => {}
                }
                std::thread::sleep(Duration::from_micros(500));
            }
            if let Some(t) = since {
                front_us += t.elapsed().as_micros() as u64;
            }
            (front_us / 1000, jumps)
        });
        Watch { stop, handle }
    }
    fn finish(self) -> (u64, u32) {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.join().unwrap_or((0, 0))
    }
}

struct Target {
    top: Hwnd,
    read: Box<dyn Fn() -> String>,
    child: Option<Child>,
}

impl Drop for Target {
    fn drop(&mut self) {
        close(self.top);
        if let Some(c) = &mut self.child {
            std::thread::sleep(Duration::from_millis(500));
            let _ = c.kill();
        }
    }
}

fn chrome(dir: &Path, mode: &str) -> Target {
    let page = dir.join("probe.html");
    std::fs::write(&page, PAGE).expect("probe.html");
    let url = format!("file:///{}#{mode}", page.display().to_string().replace('\\', "/"));
    let exe = r"C:\Program Files\Google\Chrome\Application\chrome.exe";
    let child = Command::new(exe)
        .arg(format!("--user-data-dir={}", dir.join("chrome").display()))
        .args(["--no-first-run", "--no-default-browser-check", "--new-window", "--window-position=620,80", "--window-size=760,520"])
        .arg(&url)
        .spawn()
        .expect("chrome");
    let top = find_window("probe|", None, Duration::from_secs(30)).expect("окно Chrome не появилось");
    Target {
        top,
        read: Box::new(move || {
            let t = title(top);
            t.strip_prefix("probe|").unwrap_or(&t).trim_end_matches(" - Google Chrome").to_string()
        }),
        child: Some(child),
    }
}

fn notepad(dir: &Path) -> (Target, Hwnd) {
    let file = dir.join("probe-notepad.txt");
    std::fs::write(&file, "").expect("txt");
    let child = Command::new("notepad.exe").arg(&file).spawn().expect("notepad");
    let top = find_window("probe-notepad", None, Duration::from_secs(20)).expect("окно Блокнота не появилось");
    win::bring_to_front(top);
    std::thread::sleep(Duration::from_millis(500));
    let w = win::foreground().expect("Блокнот впереди");
    let field = w.focus;
    println!("Блокнот: поле {} (класс {})", field, w.class);
    (Target { top, read: Box::new(move || edit_text(field)), child: Some(child) }, field)
}

fn vscode(dir: &Path) -> Target {
    let user = dir.join("vsc").join("User");
    std::fs::create_dir_all(&user).expect("vsc");
    std::fs::write(
        user.join("settings.json"),
        r#"{"files.autoSave":"afterDelay","files.autoSaveDelay":100,"workbench.startupEditor":"none","security.workspace.trust.enabled":false,"update.mode":"none","telemetry.telemetryLevel":"off"}"#,
    )
    .expect("settings");
    let file = dir.join("probe-vscode.txt");
    std::fs::write(&file, "").expect("txt");
    let exe = std::env::var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default().join(r"Programs\Microsoft VS Code\Code.exe");
    let child = Command::new(exe)
        .arg(format!("--user-data-dir={}", dir.join("vsc").display()))
        .arg(format!("--extensions-dir={}", dir.join("vsc-ext").display()))
        .args(["--new-window", "--disable-extensions"])
        .arg(&file)
        .spawn()
        .expect("code");
    let top = find_window("probe-vscode.txt", None, Duration::from_secs(40)).expect("окно VS Code не появилось");
    std::thread::sleep(Duration::from_secs(3));
    // свежий профиль открывает онбординг (кнопка «Continue with GitHub» с фокусом) — Esc его закрывает
    for _ in 0..2 {
        win::bring_to_front(top);
        std::thread::sleep(Duration::from_millis(300));
        if win::foreground_hwnd() == top {
            chord(&[0x1B]);
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    Target { top, read: Box::new(move || std::fs::read_to_string(&file).unwrap_or_default()), child: Some(child) }
}

/// Фейковый OpenRouter: GET — 200 `{}`, POST — через `delay` ответ `{"text":"e2e-N"}`.
fn fake_server(delay: Duration) -> (u16, Arc<AtomicU32>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("порт");
    let port = listener.local_addr().expect("адрес").port();
    let counter = Arc::new(AtomicU32::new(0));
    let c = counter.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let c = c.clone();
            std::thread::spawn(move || serve(stream, delay, c));
        }
    });
    (port, counter)
}

fn serve(mut out: TcpStream, delay: Duration, counter: Arc<AtomicU32>) {
    let Ok(input) = out.try_clone() else { return };
    let mut r = BufReader::new(input);
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let method = line.split_whitespace().next().unwrap_or("").to_string();
        let (mut len, mut chunked) = (0usize, false);
        loop {
            let mut h = String::new();
            if r.read_line(&mut h).unwrap_or(0) == 0 {
                return;
            }
            let h = h.trim_end().to_ascii_lowercase();
            if h.is_empty() {
                break;
            }
            if let Some(v) = h.strip_prefix("content-length:") {
                len = v.trim().parse().unwrap_or(0);
            }
            if h.starts_with("transfer-encoding:") && h.contains("chunked") {
                chunked = true;
            }
        }
        if chunked {
            loop {
                let mut size = String::new();
                if r.read_line(&mut size).unwrap_or(0) == 0 {
                    return;
                }
                let n = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
                let mut buf = vec![0u8; n + 2];
                if r.read_exact(&mut buf).is_err() || n == 0 {
                    break;
                }
            }
        } else {
            let mut buf = vec![0u8; len];
            if r.read_exact(&mut buf).is_err() {
                return;
            }
        }
        let body = if method == "POST" {
            std::thread::sleep(delay);
            format!("{{\"text\":\"e2e-{}\"}}", counter.fetch_add(1, Ordering::SeqCst) + 1)
        } else {
            "{}".to_string()
        };
        let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        if out.write_all(resp.as_bytes()).is_err() {
            return;
        }
    }
}

/// Сочетание держится `hold` — как человек держит хоткей, пока говорит.
fn hold_chord(keys: &[u16], hold: Duration) {
    let ev = |vk: u16, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() }, ..Default::default() },
        },
    };
    let size = std::mem::size_of::<INPUT>() as i32;
    let down: Vec<INPUT> = keys.iter().map(|k| ev(*k, false)).collect();
    let up: Vec<INPUT> = keys.iter().rev().map(|k| ev(*k, true)).collect();
    unsafe {
        SendInput(&down, size);
        std::thread::sleep(hold);
        SendInput(&up, size);
    }
}

/// Сквозной прогон через сам Verba.
fn e2e(dir: &Path, which: &str, rounds: usize) {
    let (port, answered) = fake_server(Duration::from_millis(800));
    let home = dir.join("home");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::write(
        home.join("config.json"),
        format!(
            r#"{{"hotkeys":{{"dictate":"ctrl+alt+f12","pause":"ctrl+alt+f11","paste_last":"ctrl+alt+f10"}},"openrouter":{{"base_url":"http://127.0.0.1:{port}"}},"recording":{{"trim_silence":false}},"sounds":{{"recording":false,"pause":false,"errors":false}}}}"#
        ),
    )
    .expect("config");
    let exe = std::env::current_exe().expect("exe").parent().and_then(|p| p.parent()).map(|p| p.join("verba.exe")).expect("путь");
    let mut verba = Command::new(&exe).env("VERBA_HOME", &home).spawn().expect("verba.exe — собери: cargo build --release");
    std::thread::sleep(Duration::from_secs(2));
    let person = Form::open(dir, "VerbaProbePerson", &[]);
    let (target, _form) = if which == "e2e-chrome" {
        (chrome(dir, "c"), None)
    } else {
        let f = Form::open(dir, "VerbaProbeTarget", &[]);
        let top = f.top;
        let read: Box<dyn Fn() -> String> = Box::new(move || title(top).strip_prefix("VerbaProbeTarget|").unwrap_or("").to_string());
        (Target { top, read, child: None }, Some(f))
    };
    let mut ok = 0;
    for round in 1..=rounds {
        if !wait_user_idle(Duration::from_millis(2500), Duration::from_secs(180)) {
            println!("человек работает без пауз — остановился");
            break;
        }
        win::bring_to_front(target.top);
        std::thread::sleep(Duration::from_millis(500));
        if win::foreground_hwnd() != target.top {
            println!("#{round:02} пропуск: впереди «{}»", title(win::foreground_hwnd()));
            continue;
        }
        let marker = format!("e2e-{}", answered.load(Ordering::SeqCst) + 1);
        // держит хоткей 1,2 с, отпускает и сразу уходит в своё окно; хоткей не Ctrl+Space — его
        // слушает диктовка владельца (Handy), тестовый Verba её бы перехватил
        hold_chord(&[VK_CONTROL.0, VK_MENU.0, VK_F12.0], Duration::from_millis(1200));
        std::thread::sleep(Duration::from_millis(150));
        win::bring_to_front(person.top);
        let watch = Watch::start(target.top);
        let t0 = Instant::now();
        let mut landed = false;
        while t0.elapsed() < Duration::from_secs(8) {
            if (target.read)().contains(&marker) {
                landed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(400));
        let (front_ms, times) = watch.finish();
        let fg = win::foreground_hwnd();
        let clean = !person.text().contains("e2e");
        let pass = landed && clean && fg == person.top;
        if pass {
            ok += 1;
        }
        println!(
            "#{round:02} {} {marker}: в цели {} за {} мс | человек {} | фокус {} | впереди {front_ms} мс ×{times}",
            if pass { "OK " } else { "ПРОВАЛ" },
            if landed { "да" } else { "нет" },
            t0.elapsed().as_millis(),
            if clean { "чисто" } else { "ЗАДЕТ" },
            if fg == person.top { "у человека".to_string() } else { format!("«{}»", title(fg)) },
        );
    }
    println!("итог {which}: {ok} из {rounds}");
    let _ = verba.kill();
    let _ = verba.wait();
    let log = std::fs::read_to_string(home.join("logs").join("verba.log")).unwrap_or_default();
    for line in log.lines().filter(|l| l.contains("диктовка #") || l.contains("вставка #") || l.contains("ERROR") || l.contains("WARN")) {
        println!("  лог: {line}");
    }
    drop(target);
    drop(person);
}

fn main() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let scenario = args.first().cloned().unwrap_or_else(|| "notepad".into());
    let rounds: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5);
    let dir = std::env::temp_dir().join(format!("verba-probe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    if scenario.starts_with("e2e") {
        e2e(&dir, &scenario, rounds);
        std::thread::sleep(Duration::from_millis(800));
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }

    let person = Form::open(&dir, "VerbaProbePerson", &[]);
    let mut games: Vec<String> = Vec::new();
    let mut game_form = None;
    let mut remember = true;
    let target = match scenario.as_str() {
        "notepad" => notepad(&dir).0,
        "chrome-input" => chrome(&dir, "i"),
        "chrome-textarea" => chrome(&dir, "t"),
        "chrome-editable" => chrome(&dir, "c"),
        "chrome-recreate" => chrome(&dir, "c-recreate"),
        "chrome-remove" => chrome(&dir, "c-remove"),
        "vscode" => vscode(&dir),
        "winforms" | "same" | "off" | "closed" => {
            remember = scenario != "off";
            let f = Form::open(&dir, "VerbaProbeTarget", &[]);
            let top = f.top;
            let read = {
                let t = f.title.clone();
                Box::new(move || title(top).strip_prefix(&format!("{t}|")).unwrap_or("").to_string())
            };
            game_form = Some(f);
            Target { top, read, child: None }
        }
        "game-fullscreen" | "game-list" => {
            let full = scenario == "game-fullscreen";
            let f = Form::open(&dir, "VerbaProbeGame", if full { &["-Full", "-NoBox"] } else { &["-NoBox"] });
            if !full {
                games.push("powershell.exe".into());
            }
            let top = f.top;
            let read = Box::new(move || title(top).strip_prefix("VerbaProbeGame|").unwrap_or("").to_string());
            game_form = Some(f);
            Target { top, read, child: None }
        }
        other => {
            eprintln!("нет сценария {other}");
            std::process::exit(2);
        }
    };
    let _keep = game_form;

    let (ev_tx, ev_rx) = unbounded::<Delivered>();
    let svc = TargetService::spawn(ev_tx).expect("сервис");
    let mut ok = 0;
    let mut jumps = Vec::new();
    let mut skipped = 0;
    for round in 1..=rounds {
        let marker = format!("p{round}x");
        // за ПК может работать человек: раунд — только в паузе, чтобы не забрать его фокус
        if !wait_user_idle(Duration::from_millis(2500), Duration::from_secs(180)) {
            println!("человек работает без пауз — остановился");
            break;
        }
        // курсор «человека» — в поле цели
        win::bring_to_front(target.top);
        std::thread::sleep(Duration::from_millis(400));
        if scenario == "vscode" && win::foreground_hwnd() == target.top {
            // Ctrl+1 — фокус в редактор (в свежем профиле он бывает на кнопке)
            chord(&[VK_CONTROL.0, 0x31]);
            std::thread::sleep(Duration::from_millis(300));
        }
        if std::env::var_os("PROBE_SHOW_FOCUS").is_some() {
            if let Ok(u) = verba::target::uia::Uia::new() {
                if let Some(f) = u.focused().ok().and_then(|e| u.describe(e, 0).ok()) {
                    println!("   фокус: {} · «{}» · класс «{}» · id «{}»", f.label(), f.name, f.class, f.automation_id);
                }
            }
        }
        if win::foreground_hwnd() != target.top {
            println!("#{round:02} пропуск: впереди «{}»", title(win::foreground_hwnd()));
            skipped += 1;
            continue;
        }
        let before_target = (target.read)();
        let snap = svc.capture(&games);
        std::thread::sleep(Duration::from_millis(600));
        let stay = scenario == "same" || (scenario.starts_with("game") && round % 2 == 0);
        if !stay {
            win::bring_to_front(person.top);
            std::thread::sleep(Duration::from_millis(300));
            if win::foreground_hwnd() != person.top {
                println!("#{round:02} пропуск: окно человека не встало, впереди «{}»", title(win::foreground_hwnd()));
                skipped += 1;
                continue;
            }
        }
        if scenario == "closed" {
            close(target.top);
            std::thread::sleep(Duration::from_millis(500));
        }
        let person_before = person.text();
        let watch = Watch::start(target.top);
        let (rep_tx, rep_rx) = unbounded::<PasteReport>();
        let t0 = Instant::now();
        let text = format!("{marker} ");
        if remember {
            svc.deliver(Job {
                id: round as u64,
                text,
                snapshot: snap.clone(),
                method: PasteMethod::CtrlV,
                restore_clipboard: true,
                games: games.clone(),
                done: Box::new(move |r| {
                    let _ = rep_tx.send(r);
                }),
            });
        } else {
            paste::paste(text, PasteMethod::CtrlV, true, move |r| {
                let _ = rep_tx.send(r);
            });
        }
        let delivered = if remember { ev_rx.recv_timeout(Duration::from_secs(8)).ok() } else { None };
        let needs_report = delivered.as_ref().is_none_or(|d| matches!(d.how, How::Same | How::Restored { .. }));
        let report = if needs_report { rep_rx.recv_timeout(Duration::from_secs(10)).ok() } else { None };
        let total = t0.elapsed().as_millis();
        std::thread::sleep(Duration::from_millis(400));
        let (front_ms, front_times) = watch.finish();
        let after_target = (target.read)();
        let landed = after_target.contains(&marker) && !before_target.contains(&marker);
        let person_text = person.text();
        let person_clean = person_text == person_before && !person_text.contains(&marker);
        let fg = win::foreground_hwnd();
        let fg_ok = if stay { fg == target.top } else { fg == person.top };
        let how = delivered.as_ref().map(|d| format!("{:?}", d.how)).unwrap_or_else(|| "—".into());
        let detail = delivered.as_ref().map(|d| d.detail.clone()).unwrap_or_default();
        let read = report.as_ref().map(|r| format!("{:?}", r.read_ms)).unwrap_or_else(|| "—".into());
        let expect_paste = !matches!(scenario.as_str(), "closed" | "chrome-remove") && !(scenario.starts_with("game") && !stay);
        let pass = if scenario == "off" {
            // переключатель выключен — текст в текущий фокус, то есть человеку
            !landed && person_text.contains(&marker) && fg_ok
        } else if expect_paste {
            landed && person_clean && fg_ok
        } else {
            !landed && person_clean && fg_ok
        };
        if pass {
            ok += 1;
        }
        if front_times > 0 && !stay {
            jumps.push(front_ms);
        }
        println!(
            "#{round:02} {} {how} | в цели: {} | человек: {} | фокус: {} | впереди {front_ms} мс ×{front_times} | всего {total} мс | чтение {read} | снимок {} мкс\n     {detail}",
            if pass { "OK " } else { "ПРОВАЛ" },
            if landed { "да" } else { "нет" },
            if person_clean { "чисто" } else { "ЗАДЕТ" },
            if fg_ok { "на месте".to_string() } else { format!("НЕ ТАМ: «{}»", title(fg)) },
            snap.capture_us,
        );
        if scenario == "closed" {
            break;
        }
    }
    jumps.sort_unstable();
    let max = jumps.last().copied().unwrap_or(0);
    let med = jumps.get(jumps.len() / 2).copied().unwrap_or(0);
    let tried = if scenario == "closed" { 1 } else { rounds - skipped };
    println!("итог {scenario}: {ok} из {tried} (пропущено {skipped}) · перескок медиана {med} мс, максимум {max} мс");
    drop(target);
    drop(person);
    std::thread::sleep(Duration::from_millis(800));
    let _ = std::fs::remove_dir_all(&dir);
}
