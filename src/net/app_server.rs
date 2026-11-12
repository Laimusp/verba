//! `codex app-server` — свой сервер Codex: JSON-RPC по stdio, объект на строку, без поля `jsonrpc`
//! (протокол Codex CLI 0.154; образец — QED `src/server/helpers/codex.ts:1-30, 145-215`):
//! `initialize {clientInfo, capabilities}` → уведомление `initialized` → запросы.
//! Verba спрашивает одно — `getAuthStatus {includeToken, refreshToken}` (`codex.rs` `Renewer`):
//! Codex обновляет свой вход, сам пишет новый токен в auth.json и отдаёт его.
//!
//! Процесс запускается по требованию и закрывается после минуты без запросов: stdin закрывается —
//! сервер выходит сам (~10 мс, замер QED 26.10.2026); не вышел за 2 с — дерево процессов снимается
//! через Job Object (Codex из npm — это `codex.cmd` → node → codex.exe; убить один cmd.exe мало).
//! Холодный старт — до 20 с; всё здесь асинхронно в сетевом потоке, хук и трей его не ждут
//! (CLAUDE.md §1–2). Запросы сервера к клиенту (одобрения и т. п.) без тредов не приходят; пришёл —
//! сразу ответ-ошибка, чтобы ничто не ждало Verba.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};

/// Строка без перевода строки длиннее — мусор: выбрасывается, её запрос дождётся таймаута.
const MAX_LINE: usize = 16 * 1024 * 1024;
/// Сколько stderr хранить для слов об ошибке.
const STDERR_KEEP: usize = 4096;
/// Окно консоли процессу не создаётся (`CREATE_NO_WINDOW`, документ «Process Creation Flags»):
/// Verba — оконное приложение, без флага cmd.exe и node мелькнули бы чёрным окном.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Что запускать: программа, аргументы, добавочные переменные окружения.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerCmd {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
}

impl ServerCmd {
    /// `codex app-server` найденного Codex.
    pub fn codex(program: PathBuf) -> Self {
        Self { program, args: vec!["app-server".into()], env: Vec::new() }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// без запросов дольше — процесс закрывается
    pub idle: Duration,
    /// `initialize` холодного старта (первый запуск после обновления читает конфиг и плагины)
    pub start: Duration,
    /// после закрытия stdin сервер выходит сам; жив дольше — дерево снимается
    pub exit_grace: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self { idle: Duration::from_secs(60), start: Duration::from_secs(20), exit_grace: Duration::from_secs(2) }
    }
}

type Reply = Result<Value, String>;
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Reply>>>>;

enum CallError {
    /// сервер ответил ошибкой — процесс жив
    Rpc(String),
    /// не ответил вовремя — процесс, видимо, завис
    Timeout(String),
    /// процесс вышел или его stdin закрыт
    Gone(String),
}

impl CallError {
    fn words(self) -> String {
        match self {
            CallError::Rpc(s) | CallError::Timeout(s) | CallError::Gone(s) => s,
        }
    }
}

struct Proc {
    cmd: ServerCmd,
    writer: mpsc::UnboundedSender<String>,
    pending: Pending,
    alive: Arc<AtomicBool>,
    child: Child,
    job: Option<job::Job>,
    next_id: u64,
}

impl Proc {
    async fn call(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, CallError> {
        if !self.alive.load(Ordering::SeqCst) {
            return Err(CallError::Gone("Codex app-server уже вышел".into()));
        }
        self.next_id += 1;
        let id = self.next_id;
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(id, tx);
        let line = json!({ "id": id, "method": method, "params": params }).to_string();
        if self.writer.send(line).is_err() {
            lock(&self.pending).remove(&id);
            return Err(CallError::Gone("Codex app-server закрыт".into()));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) if self.alive.load(Ordering::SeqCst) => Err(CallError::Rpc(format!("Codex ответил ошибкой на {method}: {e}"))),
            Ok(Ok(Err(e))) => Err(CallError::Gone(e)),
            Ok(Err(_)) => Err(CallError::Gone("Codex app-server вышел".into())),
            Err(_) => {
                lock(&self.pending).remove(&id);
                Err(CallError::Timeout(format!("Codex не ответил на {method} за {} с", timeout.as_secs())))
            }
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

pub struct AppServer {
    timeouts: Timeouts,
    version: String,
    inner: tokio::sync::Mutex<Option<Proc>>,
    /// растёт на каждом начале и конце запроса: сторож простоя закрывает сервер, только если
    /// после его взвода запросов не было
    epoch: AtomicU64,
    starts: AtomicU64,
}

impl AppServer {
    pub fn new(timeouts: Timeouts) -> Arc<Self> {
        Arc::new(Self {
            timeouts,
            version: env!("CARGO_PKG_VERSION").into(),
            inner: tokio::sync::Mutex::new(None),
            epoch: AtomicU64::new(0),
            starts: AtomicU64::new(0),
        })
    }

    /// Сколько раз процесс запускался (лог и тесты).
    pub fn starts(&self) -> u64 {
        self.starts.load(Ordering::SeqCst)
    }

    /// Процесс жив.
    pub async fn running(&self) -> bool {
        self.inner.lock().await.as_ref().is_some_and(|p| p.alive.load(Ordering::SeqCst))
    }

    /// Запрос; сервер запускается, если его нет (или он вышел, или программа другая).
    /// Завис на запросе — процесс снимается: следующий запрос начнёт с нового.
    pub async fn request(self: &Arc<Self>, cmd: &ServerCmd, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        let mut guard = self.inner.lock().await;
        if guard.as_ref().is_some_and(|p| !p.alive.load(Ordering::SeqCst) || p.cmd != *cmd) {
            if let Some(p) = guard.take() {
                self.retire(p);
            }
        }
        if guard.is_none() {
            match self.start(cmd).await {
                Ok(p) => *guard = Some(p),
                Err(e) => {
                    self.epoch.fetch_add(1, Ordering::SeqCst);
                    return Err(e);
                }
            }
        }
        let r = match guard.as_mut() {
            Some(p) => p.call(method, params, timeout).await,
            None => Err(CallError::Gone("Codex app-server не запущен".into())),
        };
        if matches!(r, Err(CallError::Timeout(_)) | Err(CallError::Gone(_))) {
            if let Some(p) = guard.take() {
                self.retire(p);
            }
        }
        // до отпускания лока: сторож, ждущий лок, увидит новый номер и не закроет сервер
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let alive = guard.is_some();
        drop(guard);
        if alive {
            self.arm_idle(epoch);
        }
        r.map_err(CallError::words)
    }

    /// Закрыть сейчас (выход, тесты).
    pub async fn close(&self) {
        if let Some(p) = self.inner.lock().await.take() {
            self.retire(p);
        }
    }

    fn arm_idle(self: &Arc<Self>, epoch: u64) {
        let me = Arc::downgrade(self);
        let idle = self.timeouts.idle;
        tokio::spawn(async move {
            tokio::time::sleep(idle).await;
            let Some(me) = me.upgrade() else { return };
            if me.epoch.load(Ordering::SeqCst) != epoch {
                return;
            }
            let mut g = me.inner.lock().await;
            if me.epoch.load(Ordering::SeqCst) != epoch {
                return;
            }
            if let Some(p) = g.take() {
                me.retire(p);
            }
        });
    }

    async fn start(&self, cmd: &ServerCmd) -> Result<Proc, String> {
        let mut c = Command::new(&cmd.program);
        c.args(&cmd.args)
            .envs(cmd.env.iter().map(|(k, v)| (k, v)))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .creation_flags(CREATE_NO_WINDOW);
        let mut child = c.spawn().map_err(|e| format!("Codex не запустился ({}): {e}", cmd.program.display()))?;
        self.starts.fetch_add(1, Ordering::SeqCst);
        // Job Object — сразу: cmd.exe успеет запустить node только через миллисекунды, и они
        // войдут в задание вместе с ним
        let job = child.raw_handle().and_then(job::Job::for_process);
        let (Some(stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
            return Err("Codex app-server: нет каналов stdio".into());
        };
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let (writer, rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(write_loop(stdin, rx));
        let err_task = tokio::spawn(read_stderr(stderr));
        tokio::spawn(read_loop(stdout, pending.clone(), writer.downgrade(), alive.clone(), err_task));
        let mut p = Proc { cmd: cmd.clone(), writer, pending, alive, child, job, next_id: 0 };
        let init = json!({
            "clientInfo": { "name": "verba", "title": "Verba", "version": self.version },
            "capabilities": { "experimentalApi": false, "requestAttestation": false },
        });
        match p.call("initialize", init, self.timeouts.start).await {
            Ok(_) => {
                let _ = p.writer.send(json!({ "method": "initialized" }).to_string());
                Ok(p)
            }
            Err(e) => {
                self.retire(p);
                Err(match e {
                    CallError::Timeout(_) => {
                        format!("Codex app-server не ответил на старт за {} с", self.timeouts.start.as_secs())
                    }
                    other => other.words(),
                })
            }
        }
    }

    /// Закрывает процесс в фоне: stdin закрыт — выходит сам; не вышел — дерево снимается.
    fn retire(&self, p: Proc) {
        let Proc { writer, child, job, alive, .. } = p;
        alive.store(false, Ordering::SeqCst);
        drop(writer);
        let grace = self.timeouts.exit_grace;
        tokio::spawn(async move {
            let mut child = child;
            if tokio::time::timeout(grace, child.wait()).await.is_err() {
                if let Some(j) = &job {
                    j.terminate();
                }
                let _ = child.start_kill();
                let _ = tokio::time::timeout(grace, child.wait()).await;
            }
            // KILL_ON_JOB_CLOSE: что осталось от дерева (node, codex.exe), уходит вместе с заданием
            drop(job);
        });
    }
}

async fn write_loop(mut stdin: ChildStdin, mut rx: mpsc::UnboundedReceiver<String>) {
    while let Some(mut line) = rx.recv().await {
        line.push('\n');
        if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
            break;
        }
    }
    // stdin закрывается здесь: сервер выходит сам
}

async fn read_stderr(mut err: ChildStderr) -> String {
    let mut kept = Vec::new();
    let mut chunk = [0u8; 4096];
    // читать до конца: полный канал stderr остановил бы сервер
    while let Ok(n) = err.read(&mut chunk).await {
        if n == 0 {
            break;
        }
        if kept.len() < STDERR_KEEP {
            kept.extend_from_slice(&chunk[..n.min(STDERR_KEEP - kept.len())]);
        }
    }
    String::from_utf8_lossy(&kept).to_string()
}

async fn read_loop(
    mut out: ChildStdout,
    pending: Pending,
    writer: mpsc::WeakUnboundedSender<String>,
    alive: Arc<AtomicBool>,
    err_task: tokio::task::JoinHandle<String>,
) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = match out.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        while let Some(i) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=i).collect();
            on_line(&line, &pending, &writer);
        }
        if buf.len() > MAX_LINE {
            buf.clear();
        }
    }
    alive.store(false, Ordering::SeqCst);
    let stderr = tokio::time::timeout(Duration::from_secs(1), err_task).await.ok().and_then(Result::ok).unwrap_or_default();
    let first = stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let why = if first.is_empty() { "Codex app-server вышел".to_string() } else { format!("Codex app-server вышел: {first}") };
    for (_, tx) in lock(&pending).drain() {
        let _ = tx.send(Err(why.clone()));
    }
}

fn on_line(line: &[u8], pending: &Pending, writer: &mpsc::WeakUnboundedSender<String>) {
    // строка лога на stdout — не наша
    let Ok(Value::Object(m)) = serde_json::from_slice::<Value>(line) else { return };
    if m.get("method").is_some_and(Value::is_string) {
        // запрос сервера к клиенту: ответ-ошибка сразу; уведомление — пропустить
        if let Some(id) = m.get("id").filter(|id| id.is_number() || id.is_string()) {
            if let Some(w) = writer.upgrade() {
                let _ = w.send(json!({ "id": id, "error": { "code": -32601, "message": "Verba не выполняет запросы Codex" } }).to_string());
            }
        }
        return;
    }
    let Some(id) = m.get("id").and_then(Value::as_u64) else { return };
    let Some(tx) = lock(pending).remove(&id) else { return };
    let reply = match m.get("error") {
        Some(Value::Object(e)) => Err(e.get("message").and_then(Value::as_str).unwrap_or("ошибка без текста").to_string()),
        _ => Ok(m.get("result").cloned().unwrap_or(Value::Null)),
    };
    let _ = tx.send(reply);
}

/// Где Codex: путь из настроек; иначе `codex` на PATH так, как ищет cmd.exe (каталоги по порядку,
/// расширения .com/.exe/.bat/.cmd); иначе `%APPDATA%\npm\codex.cmd` (npm ставит туда, а PATH процесса,
/// запущенного планировщиком, бывает короче). None — Codex на этом ПК нет.
pub fn find_codex(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return p.is_file().then(|| p.to_path_buf());
    }
    let exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .map(|e| e.trim().to_ascii_lowercase())
        .filter(|e| matches!(e.as_str(), ".com" | ".exe" | ".bat" | ".cmd"))
        .collect();
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            for ext in &exts {
                let p = dir.join(format!("codex{ext}"));
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    let npm = PathBuf::from(std::env::var_os("APPDATA")?).join("npm").join("codex.cmd");
    npm.is_file().then_some(npm)
}

/// Job Object с KILL_ON_JOB_CLOSE: закрытие задания (или выход Verba) снимает всё дерево Codex.
/// Документ: «Job Objects», `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`; вложенные задания — с Windows 8,
/// так что задание планировщика, в котором может идти Verba, не мешает.
mod job {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub struct Job(HANDLE);

    // хэндл задания — число, общее для процесса; Win32 разрешает звать его из любого потока
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub fn for_process(process: std::os::windows::io::RawHandle) -> Option<Job> {
            unsafe {
                let job = Job(CreateJobObjectW(None, PCWSTR::null()).ok()?);
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
                .ok()?;
                AssignProcessToJobObject(job.0, HANDLE(process)).ok()?;
                Some(job)
            }
        }

        pub fn terminate(&self) {
            unsafe {
                let _ = TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}
