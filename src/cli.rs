//! Команды из терминала: ключ OpenRouter и прокси — в Credential Manager; проверка движка Codex.
//!
//! Exe собран как оконное приложение (без консоли при автозапуске), поэтому вывод идёт в консоль
//! того терминала, откуда запустили (`AttachConsole`), а перенаправленный — в файл или канал. Ввод с клавиатуры не читаем: терминал не
//! ждёт оконное приложение и делил бы ввод с ним. Ключ берётся из аргумента или из буфера обмена.

use std::time::Duration;

use windows::core::w;
use windows::Win32::Foundation::{GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetFileType, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_WRITE, FILE_TYPE_DISK, FILE_TYPE_PIPE, OPEN_EXISTING,
};
use windows::Win32::System::Console::{AttachConsole, GetStdHandle, WriteConsoleW, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE};

use crate::config::{self, Paths, ProxyConfig, ProxyKind};
use crate::net::app_server::{find_codex, AppServer, Timeouts};
use crate::net::codex::{self, Codex, CodexSetup, Login, Renewer};
use crate::net::words::Service;
use crate::net::{detect_proxy_kind, proxy, timing_words, Hedge, OpenRouter, Route};
use crate::{paste, secrets};

const HELP: &str = "\
Verba — диктовка: держишь хоткей → говоришь → текст встаёт в поле.

  verba                    запустить (значок в трее)
  verba key set [КЛЮЧ]     сохранить ключ OpenRouter (без аргумента — из буфера обмена)
  verba key set --from-handy   взять ключ OpenRouter из настроек Handy
  verba key check          проверить сохранённый ключ
  verba key clear          удалить ключ
  verba proxy set [СТРОКА] сохранить прокси (без аргумента — из буфера обмена);
                           любая форма: socks5://логин:пароль@1.2.3.4:1080, 1.2.3.4:8080:логин:пароль…
  verba proxy clear        убрать прокси
  verba codex check [ФАЙЛ.wav]  проверить движок «ChatGPT по входу Codex»: вход, путь, ответ;
                           с файлом — прогреть соединение, распознать запись и показать задержку

Ключ и пароль прокси хранятся в Windows Credential Manager, остальное — %APPDATA%\\Verba\\config.json.";

enum Out {
    /// консоль терминала, откуда запустили
    Console(HANDLE),
    /// вывод перенаправлен в файл или канал (`verba codex check > out.txt`, скрипты) — писать туда:
    /// WriteConsoleW в консоль родителя мимо перенаправления дал бы пустой файл
    Redirected,
    None,
}

impl Out {
    fn attach() -> Out {
        unsafe {
            if let Ok(h) = GetStdHandle(STD_OUTPUT_HANDLE) {
                if !h.is_invalid() && !h.0.is_null() && matches!(GetFileType(h), FILE_TYPE_PIPE | FILE_TYPE_DISK) {
                    return Out::Redirected;
                }
            }
            if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
                return Out::None;
            }
            let h = CreateFileW(w!("CONOUT$"), GENERIC_WRITE.0, FILE_SHARE_WRITE, None, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, None);
            h.map(Out::Console).unwrap_or(Out::None)
        }
    }
    fn line(&self, s: &str) {
        match self {
            Out::Console(h) => unsafe {
                let text: Vec<u16> = format!("{s}\r\n").encode_utf16().collect();
                let _ = WriteConsoleW(*h, &text, None, None);
            },
            Out::Redirected => {
                use std::io::Write;
                let mut o = std::io::stdout().lock();
                let _ = writeln!(o, "{s}");
                let _ = o.flush();
            }
            Out::None => eprintln!("{s}"),
        }
    }
}

fn runtime() -> Option<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread().enable_all().build().ok()
}

/// Маршрут из настроек: прокси с паролем из Credential Manager.
fn route_from(cfg: &config::Config) -> Route {
    let proxy = cfg.proxy.as_ref().map(|p| {
        let kind = if p.kind == ProxyKind::Auto { ProxyKind::Http } else { p.kind };
        proxy::proxy_url(kind, &p.host, p.port, p.user.as_deref(), secrets::read(secrets::PROXY_PASSWORD).as_deref())
    });
    Route { base_url: cfg.openrouter.base_url.clone(), proxy }
}

fn check_key(out: &Out, cfg: &config::Config, key: &str) -> Option<u16> {
    let rt = runtime()?;
    let or = match OpenRouter::new(route_from(cfg)) {
        Ok(o) => o,
        Err(e) => {
            out.line(&e);
            return None;
        }
    };
    match rt.block_on(or.check_key(key, Duration::from_secs(10))) {
        Ok(s) => Some(s),
        Err(e) => {
            out.line(&format!("проверить ключ не удалось: {e}"));
            None
        }
    }
}

/// Ключ OpenRouter из настроек Handy (у владельца он там открытым текстом).
fn key_from_handy() -> Option<String> {
    let path = std::path::PathBuf::from(std::env::var_os("APPDATA")?).join("com.pais.handy").join("settings_store.json");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let s = &v["settings"];
    s["remote_transcription_api_keys"]["openrouter"]
        .as_str()
        .or_else(|| s["post_process_api_keys"]["openrouter"].as_str())
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string)
}

fn sec(ms: u64) -> String {
    format!("{:.2} с", ms as f64 / 1000.0).replace('.', ",")
}

/// `verba codex check [ФАЙЛ.wav]`: вход, прогрев, распознавание тишины или файла — с задержкой.
fn codex_check(out: &Out, cfg: &config::Config, file: Option<&str>) -> i32 {
    let auth_file = codex::auth_file();
    let program = cfg.codex.program.as_ref().map(std::path::PathBuf::from);
    match find_codex(program.as_deref()) {
        Some(p) => out.line(&format!("Codex: {}", p.display())),
        None => out.line("Codex не найден: вход не обновить, когда токен истечёт"),
    }
    match codex::read_login(&auth_file) {
        Login::Ready(a) => {
            let left = a.expires_at.map(|e| e as i64 - codex::now_secs() as i64);
            let left = match left {
                Some(s) if s > 0 => format!("токен ещё {} ч {} мин", s / 3600, s % 3600 / 60),
                Some(_) => "токен истёк — Codex обновит его при отправке".into(),
                None => "срок токена не указан".into(),
            };
            out.line(&format!("вход: ChatGPT ({}), {left}", auth_file.display()));
        }
        Login::None => out.line(&format!("входа нет ({})", auth_file.display())),
        Login::ApiKey => out.line("Codex вошёл по ключу API"),
        Login::Broken(why) => out.line(&format!("auth.json: {why}")),
    }
    let wav = match file {
        Some(f) => match std::fs::read(f) {
            Ok(w) => Some(w),
            Err(e) => {
                out.line(&format!("файл {f} не прочитать: {e}"));
                return 1;
            }
        },
        None => None,
    };
    let setup = CodexSetup { base_url: cfg.codex.base_url.clone(), proxy: route_from(cfg).proxy, auth_file, program };
    let via_proxy = setup.proxy.is_some();
    let c = match Codex::new(setup, Renewer::new(AppServer::new(Timeouts::default()))) {
        Ok(c) => c,
        Err(e) => {
            out.line(&e);
            return 1;
        }
    };
    let Some(rt) = runtime() else { return 1 };
    rt.block_on(async {
        let w = c.warm(Duration::from_secs(4)).await;
        let conn = match w.connect_ms {
            Some(ms) => format!("новое соединение {}", sec(ms)),
            None => "соединение прежнее".into(),
        };
        match (&w.error, w.status) {
            (Some(e), _) => out.line(&format!("прогрев не удался за {}: {e}", sec(w.ms))),
            (None, s) => out.line(&format!("прогрев: {conn}, ответ {} за {}", s.unwrap_or(0), sec(w.ms))),
        }
        let Some(wav) = wav else {
            return match c.check(Hedge::default()).await {
                Ok(ch) => {
                    out.line(&format!(
                        "ChatGPT по входу Codex работает: 0,3 с тишины → 200 за {}{}",
                        sec(ch.ms),
                        if ch.renewed { "; вход обновлён" } else { "" }
                    ));
                    0
                }
                Err(e) => {
                    out.line(&e);
                    1
                }
            };
        };
        let (r, trace) = c.transcribe(&wav, Hedge::default()).await;
        let login = codex::trace_words(&trace);
        if !login.is_empty() {
            out.line(&login);
        }
        match r {
            Ok(a) => {
                out.line(&format!("ответ {} за {} ({})", a.status, sec(a.ms), timing_words(&a.attempts)));
                if a.status == 200 {
                    out.line(&format!("текст: {}", a.text().unwrap_or_default()));
                    0
                } else {
                    out.line(&codex::answer_words(&a, &trace));
                    1
                }
            }
            Err(f) => {
                out.line(&f.words_at(Service::ChatGpt, via_proxy));
                1
            }
        }
    })
}

fn load_config(out: &Out, paths: &Paths) -> Option<config::Config> {
    match config::load(&paths.config_file()) {
        Ok(l) => Some(l.config),
        Err(e) => {
            out.line(&e);
            None
        }
    }
}

pub fn run(args: &[String]) -> i32 {
    let out = Out::attach();
    let paths = Paths::from_env();
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        ["key", "set", rest @ ..] => {
            let Some(cfg) = load_config(&out, &paths) else { return 2 };
            let (key, from_clipboard) = match rest {
                ["--from-handy"] => match key_from_handy() {
                    Some(k) => (k, false),
                    None => {
                        out.line("в настройках Handy ключа OpenRouter нет");
                        return 1;
                    }
                },
                [k] => (k.trim().to_string(), false),
                [] => match paste::clipboard_text() {
                    Some(t) => (t.trim().to_string(), true),
                    None => {
                        out.line("в буфере обмена нет текста → скопируй ключ OpenRouter или передай его аргументом");
                        return 1;
                    }
                },
                _ => {
                    out.line(HELP);
                    return 2;
                }
            };
            if key.len() < 20 || key.contains(char::is_whitespace) {
                out.line("это не похоже на ключ OpenRouter → нужен ключ вида sk-or-v1-… с openrouter.ai/settings/keys");
                return 1;
            }
            match check_key(&out, &cfg, &key) {
                Some(401) | Some(403) => {
                    out.line("OpenRouter не принял ключ → скопируй его заново целиком; ключ не сохранён");
                    return 1;
                }
                Some(200) => out.line("OpenRouter принял ключ"),
                Some(s) => out.line(&format!("OpenRouter ответил {s} — ключ сохраняю, проверь диктовкой")),
                None => out.line("ключ сохраняю без проверки"),
            }
            if let Err(e) = secrets::write(secrets::OPENROUTER_KEY, "openrouter", &key) {
                out.line(&e);
                return 1;
            }
            if from_clipboard {
                paste::clear_clipboard_if(&key);
            }
            out.line("ключ сохранён в Windows Credential Manager (Verba/OpenRouter)");
            0
        }
        ["key", "check"] => {
            let Some(cfg) = load_config(&out, &paths) else { return 2 };
            let Some(key) = secrets::read(secrets::OPENROUTER_KEY) else {
                out.line("ключ OpenRouter не сохранён → verba key set");
                return 1;
            };
            match check_key(&out, &cfg, &key) {
                Some(200) => {
                    out.line("ключ принят, путь до OpenRouter жив");
                    0
                }
                Some(s) => {
                    out.line(&crate::net::words::refusal(s, "", &cfg.openrouter.model));
                    1
                }
                None => 1,
            }
        }
        ["key", "clear"] => {
            secrets::delete(secrets::OPENROUTER_KEY);
            out.line("ключ OpenRouter удалён");
            0
        }
        ["proxy", "set", rest @ ..] => {
            let Some(mut cfg) = load_config(&out, &paths) else { return 2 };
            let (line, from_clipboard) = match rest {
                [] => match paste::clipboard_text() {
                    Some(t) => (t, true),
                    None => {
                        out.line("в буфере обмена нет текста → скопируй строку прокси или передай её аргументом");
                        return 1;
                    }
                },
                parts => (parts.join(" "), false),
            };
            let p = match proxy::parse(&line) {
                Ok(p) => p,
                Err(e) => {
                    out.line(&e);
                    return 1;
                }
            };
            let kind = match p.kind {
                Some(k) => k,
                None => {
                    out.line("вид прокси не назван — пробую HTTP, SOCKS5 и SOCKS4…");
                    let found = runtime().and_then(|rt| {
                        rt.block_on(detect_proxy_kind(
                            &cfg.openrouter.base_url,
                            &p.host,
                            p.port,
                            p.user.as_deref(),
                            p.pass.as_deref(),
                            Duration::from_secs(12),
                        ))
                    });
                    match found {
                        Some(k) => {
                            out.line(&format!("прокси отвечает как {k:?}"));
                            k
                        }
                        None => {
                            out.line("прокси не ответил ни как HTTP, ни как SOCKS — сохраняю, вид определится при запуске");
                            ProxyKind::Auto
                        }
                    }
                }
            };
            match &p.pass {
                Some(pass) => {
                    if let Err(e) = secrets::write(secrets::PROXY_PASSWORD, p.user.as_deref().unwrap_or(""), pass) {
                        out.line(&e);
                        return 1;
                    }
                }
                None => secrets::delete(secrets::PROXY_PASSWORD),
            }
            if from_clipboard && p.pass.is_some() {
                paste::clear_clipboard_if(line.trim());
            }
            cfg.proxy = Some(ProxyConfig { kind, host: p.host.clone(), port: p.port, user: p.user.clone() });
            if let Err(e) = config::save(&paths.config_file(), &cfg) {
                out.line(&format!("настройки не сохранены: {e}"));
                return 1;
            }
            out.line(&format!("прокси {}:{} сохранён; пароль — в Credential Manager (Verba/Proxy)", p.host, p.port));
            0
        }
        ["codex", "check"] => {
            let Some(cfg) = load_config(&out, &paths) else { return 2 };
            codex_check(&out, &cfg, None)
        }
        ["codex", "check", file] => {
            let Some(cfg) = load_config(&out, &paths) else { return 2 };
            codex_check(&out, &cfg, Some(file))
        }
        ["proxy", "clear"] => {
            let Some(mut cfg) = load_config(&out, &paths) else { return 2 };
            cfg.proxy = None;
            secrets::delete(secrets::PROXY_PASSWORD);
            if let Err(e) = config::save(&paths.config_file(), &cfg) {
                out.line(&format!("настройки не сохранены: {e}"));
                return 1;
            }
            out.line("прокси убран: Verba ходит напрямую");
            0
        }
        _ => {
            out.line(HELP);
            if matches!(a.as_slice(), ["help"] | ["--help"] | ["-h"] | ["/?"]) {
                0
            } else {
                2
            }
        }
    }
}
