//! Автозапуск с правами администратора — задача планировщика «Verba (admin)», как у Handy.
//!
//! Зачем: процесс без прав не видит клавиш окна с правами (игра от администратора) и не может слать ему
//! ввод (UIPI). Задача с `RunLevel HighestAvailable` и `LogonTrigger` запускает Verba с правами при каждом
//! входе в Windows без окна UAC; окно UAC — один раз, когда задачу создают или удаляют.
//!
//! Кто что делает (замеры и источники — `docs/windows.md` §«Автозапуск с правами»):
//! - чтение (`task_state`) и запуск (`run_task`) — COM Планировщика (`ITaskService`): без прав, без окна UAC,
//!   без дочерних процессов; «задачи нет» — точный код 0x80070002, а не разбор переведённого текста schtasks;
//! - создание и удаление — `schtasks.exe`: без прав — через одно окно UAC (`ShellExecuteExW`, глагол `runas`),
//!   уже с правами (Verba запущена задачей) — напрямую, без окна консоли.
//!
//! Всё, кроме `task_xml` и `is_elevated`, ждёт (службу планировщика, процесс schtasks, окно UAC) —
//! звать из рабочего потока, не из потока трея/UI (CLAUDE.md §2).

use std::ffi::{c_void, OsStr, OsString};
use std::fmt::Write as _;
use std::io::Read;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::core::{w, Interface, BSTR, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_CANCELLED, ERROR_FILE_NOT_FOUND, E_ACCESSDENIED, HANDLE, RPC_E_CHANGED_MODE, SCHED_E_TASK_DISABLED, WAIT_OBJECT_0,
};
use windows::Win32::Globalization::{MultiByteToWideChar, CP_OEMCP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS};
use windows::Win32::Security::{
    GetSidIdentifierAuthority, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenElevation, TokenUser, PSID,
    TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    COINIT_DISABLE_OLE1DDE,
};
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::Win32::System::TaskScheduler::{IRegisteredTask, ITaskFolder, ITaskService, TaskScheduler};
use windows::Win32::System::Threading::{GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, WaitForSingleObject};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, IShellLinkW, SHGetKnownFolderPath, ShellExecuteExW, ShellLink, KF_FLAG_DEFAULT, SEE_MASK_NOASYNC,
    SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

use crate::info;

pub const TASK_NAME: &str = "Verba (admin)";
/// Аргумент, с которым задача запускает Verba: стартовать в трей без окна.
pub const TRAY_ARG: &str = "--tray";

/// Приоритет задачи 4 = `NORMAL_PRIORITY_CLASS`. По умолчанию планировщик ставит 7: процесс ниже обычного,
/// и с ним низкие приоритеты ввода-вывода и памяти (замер — docs/windows.md).
const PRIORITY: u8 = 4;
/// schtasks отвечает за доли секунды; дольше — служба планировщика зависла.
const SCHTASKS_TIMEOUT: Duration = Duration::from_secs(30);
/// Окно консоли процессу не создаётся (документ «Process Creation Flags»).
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskState {
    Missing,
    /// `exe` — команда первого действия задачи как записана (пустой путь — у задачи нет действия «запуск программы»);
    /// `enabled` — задача не выключена в «Планировщике заданий».
    Present {
        exe: PathBuf,
        enabled: bool,
    },
}

/// Токен процесса с правами администратора (`TokenElevation`). Не ждёт.
pub fn is_elevated() -> bool {
    let Some(token) = Token::current() else { return false };
    let mut el = TOKEN_ELEVATION::default();
    let mut len = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            Some(&mut el as *mut TOKEN_ELEVATION as *mut c_void),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    }
    .is_ok();
    ok && el.TokenIsElevated != 0
}

/// XML задачи для `schtasks /create /xml`: при входе пользователя `user_id` (SID или `ДОМЕН\имя`) запустить
/// `exe --tray` с наивысшими его правами. Чистая функция, не ждёт.
pub fn task_xml(exe: &Path, user_id: &str) -> String {
    let command = esc(&exe.to_string_lossy());
    let workdir = exe
        .parent()
        .map(|d| d.to_string_lossy().into_owned())
        .filter(|d| !d.is_empty())
        .map(|d| format!("\n      <WorkingDirectory>{}</WorkingDirectory>", esc(&d)))
        .unwrap_or_default();
    let user = esc(user_id);
    // Порядок и состав — как в задаче «Handy (admin)» на этом ПК; отличия: IgnoreNew вместо Parallel (второй
    // запуск задачи не плодит экземпляры), без задержки после входа, аргумент --tray.
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Author>Verba</Author>
    <Description>Verba (диктовка) с правами администратора при входе в Windows: хоткей и вставка работают и в окнах, запущенных от администратора (игры). Включается и выключается в настройках Verba.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>{PRIORITY}</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{TRAY_ARG}</Arguments>{workdir}
    </Exec>
  </Actions>
</Task>
"#
    )
}

/// Есть ли задача «Verba (admin)» и на какой exe она смотрит. Без прав и без окна UAC: планировщик даёт
/// пользователю из `Principal` право чтения задачи, даже созданной с правами (проверено на «Handy (admin)»).
/// Ждёт службу планировщика (миллисекунды) — из рабочего потока.
pub fn task_state() -> Result<TaskState, String> {
    state_of(TASK_NAME)
}

/// Создать или заменить задачу на `exe` (полный путь). Без прав — одно окно UAC; Verba уже с правами —
/// без окна. Ждёт ответа в окне UAC сколько угодно и schtasks до 30 с — только из рабочего потока.
pub fn enable(exe: &Path) -> Result<(), String> {
    if !exe.is_absolute() {
        return Err(format!("Путь к Verba неполный ({}) → запусти Verba двойным щелчком по exe и повтори", exe.display()));
    }
    let sid = current_user_sid()?;
    let xml = TempXml::write(&task_xml(exe, &sid))?;
    let started = Instant::now();
    let how = schtasks(&create_args(TASK_NAME, xml.path()), Action::Enable)?;
    drop(xml);
    match state_of(TASK_NAME)? {
        TaskState::Present { exe: e, .. } if same_path(&e, exe) => {
            info!("автозапуск: задача «{TASK_NAME}» создана для {} ({how}, {} мс)", exe.display(), started.elapsed().as_millis());
            Ok(())
        }
        TaskState::Present { exe: e, .. } => {
            Err(format!("Задача «{TASK_NAME}» создана, но запускает {} → {}", e.display(), Action::Enable.retry()))
        }
        TaskState::Missing => Err(format!("Задача «{TASK_NAME}» не появилась в «Планировщике заданий» → {}", Action::Enable.retry())),
    }
}

/// Удалить задачу. Задачи нет — сразу Ok, без окна UAC. Без прав — одно окно UAC. Ждёт как `enable`.
pub fn disable() -> Result<(), String> {
    if let Ok(TaskState::Missing) = state_of(TASK_NAME) {
        return Ok(());
    }
    let started = Instant::now();
    let how = schtasks(&delete_args(TASK_NAME), Action::Disable)?;
    match state_of(TASK_NAME)? {
        TaskState::Missing => {
            info!("автозапуск: задача «{TASK_NAME}» удалена ({how}, {} мс)", started.elapsed().as_millis());
            Ok(())
        }
        TaskState::Present { .. } => Err(format!("Задача «{TASK_NAME}» осталась в «Планировщике заданий» → {}", Action::Disable.retry())),
    }
}

/// Запустить Verba через задачу: с правами и без окна UAC. Возвращается, как только планировщик принял
/// запуск, — новый процесс стартует следом (доли секунды). Задача `IgnoreNew`: если Verba из задачи уже
/// работает, второй не будет. Ждёт службу планировщика — из рабочего потока.
pub fn run_task() -> Result<(), String> {
    run_by_name(TASK_NAME)
}

/// Ярлык `Verba.lnk` на рабочий стол: запускает `exe`, значок — из exe. Есть — перезаписывается.
/// Ждёт COM оболочки и диск — из рабочего потока.
pub fn create_desktop_shortcut(exe: &Path) -> Result<PathBuf, String> {
    let _com = Com::enter()?;
    let lnk = desktop_dir()?.join("Verba.lnk");
    write_shortcut(exe, &lnk)?;
    Ok(lnk)
}

// ---------- планировщик через COM ----------

fn state_of(name: &str) -> Result<TaskState, String> {
    // COM — первым: выходит из него последним, после освобождения объектов
    let _com = Com::enter()?;
    let folder = root_folder()?;
    let Some(task) = find_task(&folder, name)? else { return Ok(TaskState::Missing) };
    let unreadable = |e: windows::core::Error| format!("Планировщик заданий не отдал задачу «{name}» ({e}) → повтори");
    let xml = unsafe { task.Xml() }.map_err(unreadable)?.to_string();
    let enabled = unsafe { task.Enabled() }.map_err(unreadable)?.as_bool();
    Ok(TaskState::Present { exe: parse_exe(&xml).unwrap_or_default(), enabled })
}

fn run_by_name(name: &str) -> Result<(), String> {
    let _com = Com::enter()?;
    let folder = root_folder()?;
    let task =
        find_task(&folder, name)?.ok_or_else(|| format!("Задачи «{name}» нет → включи «Автозапуск с правами» в настройках Verba"))?;
    // планировщик сам о пропавшем exe не скажет: Run вернёт успех, а запуск упадёт молча (код 0x80070002 в задаче)
    if let Some(exe) = unsafe { task.Xml() }.ok().and_then(|x| parse_exe(&x.to_string())) {
        if exe.is_absolute() && !exe.is_file() {
            return Err(format!(
                "Задача «{name}» запускает {}, а этого файла нет → включи «Автозапуск с правами» ещё раз в той Verba, которой пользуешься",
                exe.display()
            ));
        }
    }
    unsafe { task.Run(&VARIANT::default()) }.map(|_| ()).map_err(|e| {
        if e.code() == SCHED_E_TASK_DISABLED {
            format!("Задача «{name}» выключена в «Планировщике заданий» → включи «Автозапуск с правами» в настройках Verba ещё раз")
        } else if e.code() == E_ACCESSDENIED {
            format!("Windows не дала запустить задачу «{name}» → включи «Автозапуск с правами» в настройках Verba ещё раз")
        } else {
            format!("Планировщик заданий не запустил «{name}» ({e}) → повтори; не выйдет — включи «Автозапуск с правами» ещё раз")
        }
    })
}

fn root_folder() -> Result<ITaskFolder, String> {
    let down = |e: windows::core::Error| {
        format!("Планировщик заданий Windows не отвечает ({e}) → проверь, что служба «Планировщик заданий» работает, и повтори")
    };
    unsafe {
        let svc: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER).map_err(down)?;
        let none = VARIANT::default();
        svc.Connect(&none, &none, &none, &none).map_err(down)?;
        svc.GetFolder(&BSTR::from("\\")).map_err(down)
    }
}

fn find_task(folder: &ITaskFolder, name: &str) -> Result<Option<IRegisteredTask>, String> {
    match unsafe { folder.GetTask(&BSTR::from(name)) } {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() => Ok(None),
        Err(e) if e.code() == E_ACCESSDENIED => Err(format!(
            "Задачу «{name}» не прочитать без прав администратора → удали её в «Планировщике заданий» и включи «Автозапуск с правами» в Verba заново"
        )),
        Err(e) => Err(format!("Планировщик заданий не отдал задачу «{name}» ({e}) → повтори")),
    }
}

/// COM на время вызова: STA с `COINIT_DISABLE_OLE1DDE`, как велит документ `ShellExecuteEx`. Поток уже в MTA
/// (`RPC_E_CHANGED_MODE`) — работаем в нём: объекты планировщика и ярлыка живут в любом апартаменте.
struct Com {
    owned: bool,
}

impl Com {
    fn enter() -> Result<Com, String> {
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        if hr == RPC_E_CHANGED_MODE {
            return Ok(Com { owned: false });
        }
        hr.ok().map_err(|e| format!("COM не запустился ({e}) → перезапусти Verba и повтори"))?;
        // S_OK и S_FALSE (уже был в STA) — оба требуют парного CoUninitialize
        Ok(Com { owned: true })
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.owned {
            unsafe { CoUninitialize() };
        }
    }
}

// ---------- schtasks: создание и удаление ----------

#[derive(Clone, Copy)]
enum Action {
    Enable,
    Disable,
}

impl Action {
    fn retry(self) -> &'static str {
        match self {
            Action::Enable => "включи «Автозапуск с правами» ещё раз",
            Action::Disable => "выключи «Автозапуск с правами» ещё раз",
        }
    }
}

fn create_args(name: &str, xml: &Path) -> Vec<String> {
    vec!["/create".into(), "/tn".into(), name.into(), "/xml".into(), xml.to_string_lossy().into_owned(), "/f".into()]
}

fn delete_args(name: &str) -> Vec<String> {
    vec!["/delete".into(), "/tn".into(), name.into(), "/f".into()]
}

/// schtasks с правами: Verba уже с правами — напрямую, иначе через окно UAC. Ok — как получилось (для лога).
fn schtasks(args: &[String], action: Action) -> Result<&'static str, String> {
    if is_elevated() {
        schtasks_direct(args, action).map(|_| "с правами, без UAC")
    } else {
        schtasks_runas(args, action).map(|_| "через окно UAC")
    }
}

/// `%SystemRoot%\System32\schtasks.exe` — полным путём, не через PATH.
fn schtasks_exe() -> PathBuf {
    let mut buf = [0u16; 260];
    let n = unsafe { GetSystemDirectoryW(Some(&mut buf)) } as usize;
    let dir = if n > 0 && n < buf.len() { PathBuf::from(OsString::from_wide(&buf[..n])) } else { PathBuf::from(r"C:\Windows\System32") };
    dir.join("schtasks.exe")
}

/// schtasks в этом же процессе-родителе, без окна консоли; его ответ — в тексте ошибки.
fn schtasks_direct(args: &[String], action: Action) -> Result<(), String> {
    let mut child = Command::new(schtasks_exe())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("schtasks не запустился ({e}) → {}", action.retry()))?;
    let deadline = Instant::now() + SCHTASKS_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(timeout_words(action));
            }
            Err(e) => {
                let _ = child.kill();
                return Err(format!("schtasks: {e} → {}", action.retry()));
            }
        }
    };
    if status.success() {
        return Ok(());
    }
    // ответ schtasks — одна-две строки: целиком лежит в буфере канала, пока ждём выхода
    let mut out = Vec::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_end(&mut out);
    }
    if out.is_empty() {
        if let Some(mut o) = child.stdout.take() {
            let _ = o.read_to_end(&mut out);
        }
    }
    // без консоли schtasks пишет в OEM-кодировке (cp866 на русской Windows) и на языке системы
    let text = one_line(&oem_text(&out));
    let text = if text.is_empty() { format!("код {}", status.code().unwrap_or(-1)) } else { text };
    Err(format!("Планировщик заданий отказал: {text} → {}", action.retry()))
}

/// schtasks с правами через окно UAC. Вывод недоступен (ShellExecuteEx не перенаправляет его) — только код
/// выхода; итог проверяет вызывающий чтением задачи.
fn schtasks_runas(args: &[String], action: Action) -> Result<(), String> {
    shell_execute_schtasks(w!("runas"), args, action)
}

/// schtasks через ShellExecuteEx с глаголом `verb`, ждать выхода, взять код. Глагол `open` — только для
/// живой проверки всего пути, кроме самого окна UAC.
fn shell_execute_schtasks(verb: PCWSTR, args: &[String], action: Action) -> Result<(), String> {
    let _com = Com::enter()?;
    let file = wide_os(schtasks_exe().as_os_str());
    let params = wide(&command_line(args));
    let mut sei = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: verb,
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        // консоль schtasks скрыта: проверено на ShellExecuteEx без runas — ни окна консоли, ни Терминала
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    // возвращается, когда человек ответил в окне UAC
    if let Err(e) = unsafe { ShellExecuteExW(&mut sei) } {
        return Err(if e.code() == ERROR_CANCELLED.to_hresult() {
            format!("Windows не дала прав: в окне UAC нажали «Нет» → {} и нажми «Да»", action.retry())
        } else {
            format!("Windows не запустила schtasks с правами ({e}) → {}", action.retry())
        });
    }
    if sei.hProcess.is_invalid() {
        return Err(format!("Windows не отдала процесс schtasks → {}", action.retry()));
    }
    let process = Owned(sei.hProcess);
    if unsafe { WaitForSingleObject(process.0, SCHTASKS_TIMEOUT.as_millis() as u32) } != WAIT_OBJECT_0 {
        return Err(timeout_words(action));
    }
    let mut code = 0u32;
    unsafe { GetExitCodeProcess(process.0, &mut code) }.map_err(|e| format!("schtasks: {e} → {}", action.retry()))?;
    if code == 0 {
        Ok(())
    } else {
        Err(format!("Планировщик заданий отказал (schtasks с правами, код {code}) → {}", action.retry()))
    }
}

fn timeout_words(action: Action) -> String {
    format!(
        "schtasks не ответил за {} с → проверь, что служба «Планировщик заданий» работает, и {}",
        SCHTASKS_TIMEOUT.as_secs(),
        action.retry()
    )
}

/// Строка параметров по правилам разбора командной строки MSVC (как у `CommandLineToArgvW`).
fn command_line(args: &[String]) -> String {
    args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
}

fn quote(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '"']) {
        return a.to_string();
    }
    let mut out = String::from('"');
    let mut slashes = 0usize;
    for c in a.chars() {
        match c {
            '\\' => {
                slashes += 1;
                continue;
            }
            // косые перед кавычкой удваиваются, сама кавычка экранируется
            '"' => {
                out.push_str(&"\\".repeat(slashes * 2 + 1));
                out.push('"');
            }
            _ => {
                out.push_str(&"\\".repeat(slashes));
                out.push(c);
            }
        }
        slashes = 0;
    }
    // косые перед закрывающей кавычкой — тоже
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    out
}

fn oem_text(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    unsafe {
        let n = MultiByteToWideChar(CP_OEMCP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), bytes, None);
        if n <= 0 {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        let mut buf = vec![0u16; n as usize];
        let n = MultiByteToWideChar(CP_OEMCP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), bytes, Some(&mut buf));
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

/// Ответ schtasks одной строкой: строки через пробел (у schtasks бывает `\r\r\n`).
fn one_line(s: &str) -> String {
    s.split(['\r', '\n']).map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ")
}

/// XML задачи во временном файле: UTF-16 LE с BOM — иначе schtasks отвечает «XML-код задачи неверно записан»
/// (проверено: UTF-16 без BOM и UTF-8 с BOM и без — отказ). Файл удаляется при выходе из области.
struct TempXml(PathBuf);

impl TempXml {
    fn write(xml: &str) -> Result<TempXml, String> {
        use std::io::Write;
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let path = std::env::temp_dir().join(format!("verba-task-{}-{nanos}.xml", std::process::id()));
        let mut bytes = Vec::with_capacity(2 + xml.len() * 2);
        bytes.extend_from_slice(&[0xFF, 0xFE]);
        for u in xml.encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("Временный файл задачи не создан ({e}) → проверь место на диске и папку TEMP, и повтори"))?;
        let tmp = TempXml(path);
        f.write_all(&bytes).map_err(|e| format!("Временный файл задачи не записан ({e}) → проверь место на диске и повтори"))?;
        Ok(tmp)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempXml {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ---------- XML ----------

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail.find(';');
        let ch = end.and_then(|e| match &tail[1..e] {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            n if n.starts_with("#x") || n.starts_with("#X") => u32::from_str_radix(&n[2..], 16).ok().and_then(char::from_u32),
            n if n.starts_with('#') => n[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        });
        match (ch, end) {
            (Some(c), Some(e)) => {
                out.push(c);
                rest = &tail[e + 1..];
            }
            _ => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Содержимое первого элемента `<tag …>…</tag>`.
fn element<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}");
    let mut from = 0;
    loop {
        let at = from + xml[from..].find(&open)?;
        let after = at + open.len();
        // <Exec> или <Exec id="…">, но не <ExecSomething>
        match xml[after..].chars().next()? {
            '>' | ' ' | '\t' | '\r' | '\n' => {
                let body = after + xml[after..].find('>')? + 1;
                let close = body + xml[body..].find(&format!("</{tag}>"))?;
                return Some(&xml[body..close]);
            }
            _ => from = after,
        }
    }
}

/// Программа задачи: `<Command>` первого действия `<Exec>`, без кавычек вокруг и с раскрытыми `&amp;`…
/// Планировщик хранит команду как её ввели: бывает и в кавычках.
fn parse_exe(xml: &str) -> Option<PathBuf> {
    let cmd = unescape(element(element(xml, "Exec")?, "Command")?);
    let cmd = cmd.trim();
    let cmd = cmd.strip_prefix('"').and_then(|c| c.strip_suffix('"')).unwrap_or(cmd).trim();
    (!cmd.is_empty()).then(|| PathBuf::from(cmd))
}

fn same_path(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

// ---------- пользователь, ярлык, мелочи Win32 ----------

struct Token(HANDLE);

impl Token {
    fn current() -> Option<Token> {
        let mut h = HANDLE::default();
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut h) }.ok()?;
        Some(Token(h))
    }
}

impl Drop for Token {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// SID пользователя процесса строкой. SID, а не `ДОМЕН\имя`: переживает переименование учётной записи;
/// планировщик принимает его и в `Principal`, и в `LogonTrigger` (проверено).
fn current_user_sid() -> Result<String, String> {
    let fail = || "Windows не назвала пользователя процесса → перезапусти Verba и повтори".to_string();
    let token = Token::current().ok_or_else(fail)?;
    let mut len = 0u32;
    unsafe {
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut len);
    }
    if (len as usize) < size_of::<TOKEN_USER>() {
        return Err(fail());
    }
    // u64 — выравнивание под TOKEN_USER (внутри указатель)
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    unsafe { GetTokenInformation(token.0, TokenUser, Some(buf.as_mut_ptr().cast()), len, &mut len) }.map_err(|_| fail())?;
    let user = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
    Ok(unsafe { sid_string(user.User.Sid) })
}

unsafe fn sid_string(sid: PSID) -> String {
    unsafe {
        let revision = *(sid.0 as *const u8);
        let authority = (*GetSidIdentifierAuthority(sid)).Value;
        let count = *GetSidSubAuthorityCount(sid) as u32;
        let subs: Vec<u32> = (0..count).map(|i| *GetSidSubAuthority(sid, i)).collect();
        format_sid(revision, authority, &subs)
    }
}

/// `S-R-I-S-S…` по документу «SID Components»: авторитет меньше 2^32 — десятичным, иначе `0x` и 12 цифр.
fn format_sid(revision: u8, authority: [u8; 6], subs: &[u32]) -> String {
    let mut s = format!("S-{revision}-");
    if authority[0] == 0 && authority[1] == 0 {
        let _ = write!(s, "{}", u32::from_be_bytes([authority[2], authority[3], authority[4], authority[5]]));
    } else {
        s.push_str("0x");
        for b in authority {
            let _ = write!(s, "{b:02X}");
        }
    }
    for x in subs {
        let _ = write!(s, "-{x}");
    }
    s
}

fn desktop_dir() -> Result<PathBuf, String> {
    unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None)
            .map_err(|e| format!("Windows не назвала папку рабочего стола ({e}) → повтори"))?;
        let dir = PathBuf::from(OsString::from_wide(p.as_wide()));
        CoTaskMemFree(Some(p.0 as *const c_void));
        Ok(dir)
    }
}

/// Ярлык на exe: рабочая папка — папка exe, значок — первый из exe. COM открыт вызывающим.
fn write_shortcut(exe: &Path, lnk: &Path) -> Result<(), String> {
    let fail = |e: windows::core::Error| format!("Ярлык Verba не создан ({e}) → проверь, что в {} можно писать, и повтори", lnk.display());
    let exe_w = wide_os(exe.as_os_str());
    let desc = wide("Verba — диктовка: держишь хоткей, говоришь, текст встаёт в поле");
    let lnk_w = wide_os(lnk.as_os_str());
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(fail)?;
        link.SetPath(PCWSTR(exe_w.as_ptr())).map_err(fail)?;
        if let Some(dir) = exe.parent() {
            let dir_w = wide_os(dir.as_os_str());
            link.SetWorkingDirectory(PCWSTR(dir_w.as_ptr())).map_err(fail)?;
        }
        link.SetIconLocation(PCWSTR(exe_w.as_ptr()), 0).map_err(fail)?;
        link.SetDescription(PCWSTR(desc.as_ptr())).map_err(fail)?;
        let file: IPersistFile = link.cast().map_err(fail)?;
        file.Save(PCWSTR(lnk_w.as_ptr()), true).map_err(fail)?;
    }
    Ok(())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_os(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "S-1-5-21-1111111111-2222222222-3333333333-1001";

    #[test]
    fn xml_has_every_setting_of_the_task() {
        let x = task_xml(Path::new(r"C:\Users\u\Desktop\Verba.exe"), SID);
        assert!(x.starts_with(r#"<?xml version="1.0" encoding="UTF-16"?>"#));
        for need in [
            "<RunLevel>HighestAvailable</RunLevel>",
            "<LogonType>InteractiveToken</LogonType>",
            "<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>",
            "<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>",
            "<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>",
            "<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>",
            "<Priority>4</Priority>",
            "<AllowStartOnDemand>true</AllowStartOnDemand>",
            r"<Command>C:\Users\u\Desktop\Verba.exe</Command>",
            "<Arguments>--tray</Arguments>",
            r"<WorkingDirectory>C:\Users\u\Desktop</WorkingDirectory>",
        ] {
            assert!(x.contains(need), "нет {need}");
        }
        // вход именно этого пользователя: без UserId в триггере задача сработала бы на вход любого
        let user = format!("<UserId>{SID}</UserId>");
        assert!(element(&x, "LogonTrigger").unwrap().contains(&user));
        assert!(element(&x, "Principal").unwrap().contains(&user));
        assert!(!x.contains("<Delay>"));
    }

    #[test]
    fn path_is_escaped_and_read_back() {
        let exe = Path::new(r"C:\Temp\A & B's <x>\verba.exe");
        let x = task_xml(exe, SID);
        assert!(x.contains(r"<Command>C:\Temp\A &amp; B&apos;s &lt;x&gt;\verba.exe</Command>"));
        assert!(x.contains(r"<WorkingDirectory>C:\Temp\A &amp; B&apos;s &lt;x&gt;</WorkingDirectory>"));
        assert_eq!(parse_exe(&x).unwrap(), exe);
    }

    #[test]
    fn exe_from_scheduler_export() {
        // так задачу отдаёт планировщик: апостроф не экранирован, у Exec бывает id, команда бывает в кавычках
        let x = r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.3" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Settings><Enabled>false</Enabled><Priority>4</Priority></Settings>
  <Actions Context="Author">
    <Exec id="a1">
      <Command>"C:\Program Files\A &amp; B's\verba.exe"</Command>
      <Arguments>--tray</Arguments>
    </Exec>
  </Actions>
</Task>"#;
        assert_eq!(parse_exe(x).unwrap(), Path::new(r"C:\Program Files\A & B's\verba.exe"));
        let handy = r#"<Actions Context="Author"><Exec><Command>C:\Users\u\AppData\Local\Handy\handy.exe</Command><WorkingDirectory>C:\Users\u\AppData\Local\Handy</WorkingDirectory></Exec></Actions>"#;
        assert_eq!(parse_exe(handy).unwrap(), Path::new(r"C:\Users\u\AppData\Local\Handy\handy.exe"));
    }

    #[test]
    fn task_without_program_has_no_exe() {
        assert_eq!(parse_exe(r#"<Actions><ComHandler><ClassId>{x}</ClassId></ComHandler></Actions>"#), None);
        assert_eq!(parse_exe("<Actions><Exec><Command>  </Command></Exec></Actions>"), None);
        // <ExecutionTimeLimit> — не <Exec>
        assert_eq!(parse_exe("<Settings><ExecutionTimeLimit>PT0S</ExecutionTimeLimit></Settings>"), None);
    }

    #[test]
    fn entities() {
        assert_eq!(unescape("a &amp; b &lt;c&gt; &quot;d&quot; &apos;e&apos; &#1071; &#x42F;"), "a & b <c> \"d\" 'e' Я Я");
        assert_eq!(unescape("A & B; &bogus; &"), "A & B; &bogus; &");
    }

    #[test]
    fn sid_strings() {
        assert_eq!(format_sid(1, [0, 0, 0, 0, 0, 5], &[21, 1111111111, 2222222222, 3333333333, 1001]), SID);
        assert_eq!(format_sid(1, [0, 0, 0, 0, 0, 5], &[18]), "S-1-5-18");
        assert_eq!(format_sid(1, [0, 1, 0, 0, 0, 0], &[7]), "S-1-0x000100000000-7");
    }

    #[test]
    fn sid_of_this_process() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-5-"), "{sid}");
    }

    #[test]
    fn command_line_quoting() {
        let args = create_args("Verba (admin)", Path::new(r"C:\Users\A B\AppData\Local\Temp\verba-task-1-2.xml"));
        assert_eq!(command_line(&args), r#"/create /tn "Verba (admin)" /xml "C:\Users\A B\AppData\Local\Temp\verba-task-1-2.xml" /f"#);
        assert_eq!(quote(r"C:\dir with space\"), r#""C:\dir with space\\""#);
        assert_eq!(quote(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(quote(""), r#""""#);
        assert_eq!(quote(r"C:\plain\x.xml"), r"C:\plain\x.xml");
    }

    #[test]
    fn schtasks_answer_in_one_line() {
        assert_eq!(
            one_line("ОШИБКА: XML-код задачи неверно записан.\r\r\n(1,2)::ОШИБКА: один корневой элемент\r\n"),
            "ОШИБКА: XML-код задачи неверно записан. (1,2)::ОШИБКА: один корневой элемент"
        );
        assert_eq!(oem_text(b"ERROR: Access is denied.\r\n"), "ERROR: Access is denied.\r\n");
        assert_eq!(oem_text(b""), "");
    }

    #[test]
    fn xml_file_is_utf16le_with_bom_and_removed() {
        let f = TempXml::write("<a>Я</a>").unwrap();
        let path = f.path().to_path_buf();
        let b = std::fs::read(&path).unwrap();
        assert_eq!(&b[..4], &[0xFF, 0xFE, b'<', 0]);
        assert_eq!(b.len(), 2 + "<a>Я</a>".encode_utf16().count() * 2);
        drop(f);
        assert!(!path.exists());
    }

    #[test]
    fn shortcut_points_to_exe_with_its_icon() {
        let exe = std::env::current_exe().unwrap();
        let dir = std::env::temp_dir().join(format!("verba-lnk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let lnk = dir.join("Verba.lnk");
        let _com = Com::enter().unwrap();
        write_shortcut(&exe, &lnk).unwrap();
        unsafe {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
            let file: IPersistFile = link.cast().unwrap();
            let lnk_w = wide_os(lnk.as_os_str());
            file.Load(PCWSTR(lnk_w.as_ptr()), windows::Win32::System::Com::STGM_READ).unwrap();
            let mut buf = [0u16; 1024];
            link.GetPath(&mut buf, std::ptr::null_mut(), 0).unwrap();
            let got = PathBuf::from(OsString::from_wide(&buf[..buf.iter().position(|&c| c == 0).unwrap()]));
            assert!(same_path(&got, &exe), "{} ≠ {}", got.display(), exe.display());
            let mut icon = [0u16; 1024];
            let mut index = -1;
            link.GetIconLocation(&mut icon, &mut index).unwrap();
            let icon = PathBuf::from(OsString::from_wide(&icon[..icon.iter().position(|&c| c == 0).unwrap()]));
            assert!(same_path(&icon, &exe));
            assert_eq!(index, 0);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- живые проверки: трогают Планировщик заданий этого ПК ----
    // cargo test --lib autostart -- --ignored --nocapture
    // Задача «Verba (test)» создаётся без прав (LeastPrivilege) и удаляется в конце, в том числе при панике.

    const TEST_TASK: &str = "Verba (test)";

    struct DeleteTestTask;

    impl Drop for DeleteTestTask {
        fn drop(&mut self) {
            let _ = schtasks_direct(&delete_args(TEST_TASK), Action::Disable);
        }
    }

    #[test]
    #[ignore = "трогает Планировщик заданий этого ПК"]
    fn live_missing_task_is_missing() {
        assert_eq!(state_of("Verba (нет такой задачи 7f3a)").unwrap(), TaskState::Missing);
        let e = run_by_name("Verba (нет такой задачи 7f3a)").unwrap_err();
        assert!(e.contains("нет →"), "{e}");
    }

    #[test]
    #[ignore = "трогает Планировщик заданий этого ПК"]
    fn live_round_trip_without_rights() {
        let sid = current_user_sid().unwrap();
        let fake = Path::new(r"C:\Temp\Verba & test's\verba.exe");
        if !is_elevated() {
            // HighestAvailable без прав планировщик не принимает — отсюда окно UAC в enable()
            let f = TempXml::write(&task_xml(fake, &sid)).unwrap();
            let e = schtasks_direct(&create_args(TEST_TASK, f.path()), Action::Enable).unwrap_err();
            println!("HighestAvailable без прав: {e}");
            assert!(!e.contains('\u{FFFD}'), "ответ schtasks не раскодирован: {e}");
            assert_eq!(state_of(TEST_TASK).unwrap(), TaskState::Missing);
        }
        let _cleanup = DeleteTestTask;
        let least = |exe: &Path| task_xml(exe, &sid).replace("HighestAvailable", "LeastPrivilege");

        // путь с & и ' — туда и обратно; exe нет → запуск отказывает словами, не запуская
        let f = TempXml::write(&least(fake)).unwrap();
        schtasks_direct(&create_args(TEST_TASK, f.path()), Action::Enable).unwrap();
        assert_eq!(state_of(TEST_TASK).unwrap(), TaskState::Present { exe: fake.into(), enabled: true });
        let e = run_by_name(TEST_TASK).unwrap_err();
        assert!(e.contains("этого файла нет"), "{e}");

        // настоящий exe, задача выключена → запуск отказывает словами
        let exe = std::env::current_exe().unwrap();
        let f = TempXml::write(&least(&exe)).unwrap();
        schtasks_direct(&create_args(TEST_TASK, f.path()), Action::Enable).unwrap();
        let args: Vec<String> = ["/change", "/tn", TEST_TASK, "/disable"].map(String::from).into();
        schtasks_direct(&args, Action::Enable).unwrap();
        assert_eq!(state_of(TEST_TASK).unwrap(), TaskState::Present { exe: exe.clone(), enabled: false });
        let e = run_by_name(TEST_TASK).unwrap_err();
        assert!(e.contains("выключена"), "{e}");

        schtasks_direct(&delete_args(TEST_TASK), Action::Disable).unwrap();
        assert_eq!(state_of(TEST_TASK).unwrap(), TaskState::Missing);
    }

    /// Путь окна UAC целиком, кроме самого окна: ShellExecuteEx с глаголом `open` вместо `runas` —
    /// кавычки в параметрах (имя задачи со скобками, папка с пробелом и &), скрытая консоль, ожидание, код выхода.
    #[test]
    #[ignore = "трогает Планировщик заданий этого ПК"]
    fn live_shell_execute_path_without_uac() {
        let sid = current_user_sid().unwrap();
        let dir = std::env::temp_dir().join(format!("verba test {} & co", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let xml_at = |name: &str, xml: &str| {
            let p = dir.join(name);
            let mut b = vec![0xFF, 0xFE];
            xml.encode_utf16().for_each(|u| b.extend_from_slice(&u.to_le_bytes()));
            std::fs::write(&p, b).unwrap();
            p
        };
        let exe = std::env::current_exe().unwrap();
        let _cleanup = DeleteTestTask;

        if !is_elevated() {
            // отказ планировщика виден кодом выхода: HighestAvailable без прав
            let hi = xml_at("hi.xml", &task_xml(&exe, &sid));
            let e = shell_execute_schtasks(w!("open"), &create_args(TEST_TASK, &hi), Action::Enable).unwrap_err();
            println!("отказ через ShellExecuteEx: {e}");
            assert!(e.contains("код 1"), "{e}");
        }

        let least = xml_at("least.xml", &task_xml(&exe, &sid).replace("HighestAvailable", "LeastPrivilege"));
        let started = Instant::now();
        shell_execute_schtasks(w!("open"), &create_args(TEST_TASK, &least), Action::Enable).unwrap();
        println!("создание через ShellExecuteEx: {} мс", started.elapsed().as_millis());
        assert_eq!(state_of(TEST_TASK).unwrap(), TaskState::Present { exe: exe.clone(), enabled: true });
        shell_execute_schtasks(w!("open"), &delete_args(TEST_TASK), Action::Disable).unwrap();
        assert_eq!(state_of(TEST_TASK).unwrap(), TaskState::Missing);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
