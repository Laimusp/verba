//! Команды из терминала: ключ OpenRouter и прокси — в Credential Manager.
//!
//! Exe собран как оконное приложение (без консоли при автозапуске), поэтому вывод идёт в консоль
//! того терминала, откуда запустили (`AttachConsole`). Ввод с клавиатуры не читаем: терминал не
//! ждёт оконное приложение и делил бы ввод с ним. Ключ берётся из аргумента или из буфера обмена.

use std::time::Duration;

use windows::core::w;
use windows::Win32::Foundation::{GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows::Win32::System::Console::{AttachConsole, WriteConsoleW, ATTACH_PARENT_PROCESS};

use crate::config::{self, Paths, ProxyConfig, ProxyKind};
use crate::net::{detect_proxy_kind, proxy, OpenRouter, Route};
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

Ключ и пароль прокси хранятся в Windows Credential Manager, остальное — %APPDATA%\\Verba\\config.json.";

struct Out(Option<HANDLE>);

impl Out {
    fn attach() -> Out {
        unsafe {
            if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
                return Out(None);
            }
            let h = CreateFileW(w!("CONOUT$"), GENERIC_WRITE.0, FILE_SHARE_WRITE, None, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, None);
            Out(h.ok())
        }
    }
    fn line(&self, s: &str) {
        let text: Vec<u16> = format!("{s}\r\n").encode_utf16().collect();
        match self.0 {
            Some(h) => unsafe {
                let _ = WriteConsoleW(h, &text, None, None);
            },
            None => eprintln!("{s}"),
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
