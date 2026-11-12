//! Лог: строки уходят каналом в свой поток, он пишет на диск и ротирует файл.
//! Поток трея и контроллер на диск не ждут (CLAUDE.md §2); хук не логирует вовсе (§1).
//! Ключи, токены и тексты диктовок в лог не пишутся (§6) — это правило вызывающего кода.

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

use crossbeam_channel::{Receiver, Sender};

/// Файл больше — переименовывается в verba.1.log (старые сдвигаются), пишется новый.
const MAX_BYTES: u64 = 2 * 1024 * 1024;
const KEEP: usize = 3;

enum Msg {
    Line(String),
    Flush(Sender<()>),
}

static TX: OnceLock<Sender<Msg>> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

fn stamp() -> String {
    // местное время: лог читает человек
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

pub fn write(level: Level, msg: impl AsRef<str>) {
    let tag = match level {
        Level::Info => "",
        Level::Warn => "ВНИМАНИЕ ",
        Level::Error => "ОШИБКА ",
    };
    let line = format!("{} {}{}\n", stamp(), tag, msg.as_ref());
    match TX.get() {
        Some(tx) => {
            let _ = tx.send(Msg::Line(line));
        }
        None => eprint!("{line}"),
    }
}

/// Дописать всё, что в очереди (перед выходом).
pub fn flush() {
    if let Some(tx) = TX.get() {
        let (ack_tx, ack_rx) = crossbeam_channel::bounded(1);
        if tx.send(Msg::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(std::time::Duration::from_secs(2));
        }
    }
}

#[macro_export]
macro_rules! info {
    ($($t:tt)*) => { $crate::logger::write($crate::logger::Level::Info, format!($($t)*)) };
}
#[macro_export]
macro_rules! warn {
    ($($t:tt)*) => { $crate::logger::write($crate::logger::Level::Warn, format!($($t)*)) };
}
#[macro_export]
macro_rules! error {
    ($($t:tt)*) => { $crate::logger::write($crate::logger::Level::Error, format!($($t)*)) };
}

pub fn init(path: PathBuf) {
    let (tx, rx) = crossbeam_channel::unbounded();
    if TX.set(tx).is_err() {
        return;
    }
    let _ = std::thread::Builder::new().name("verba-log".into()).spawn(move || run(path, rx));
}

fn rotated(path: &std::path::Path, n: usize) -> PathBuf {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("verba");
    path.with_file_name(format!("{stem}.{n}.log"))
}

fn rotate(path: &std::path::Path) {
    let _ = std::fs::remove_file(rotated(path, KEEP));
    for n in (1..KEEP).rev() {
        let _ = std::fs::rename(rotated(path, n), rotated(path, n + 1));
    }
    let _ = std::fs::rename(path, rotated(path, 1));
}

fn open(path: &std::path::Path) -> Option<(std::fs::File, u64)> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let f = std::fs::OpenOptions::new().create(true).append(true).open(path).ok()?;
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    Some((f, len))
}

fn run(path: PathBuf, rx: Receiver<Msg>) {
    let mut file = open(&path);
    for msg in rx {
        match msg {
            Msg::Line(line) => {
                if file.as_ref().is_some_and(|(_, len)| *len >= MAX_BYTES) {
                    file = None;
                    rotate(&path);
                }
                if file.is_none() {
                    file = open(&path);
                }
                if let Some((f, len)) = file.as_mut() {
                    if f.write_all(line.as_bytes()).is_ok() {
                        *len += line.len() as u64;
                    } else {
                        // диск отвалился или файл удалили — открыть заново на следующей строке
                        file = None;
                    }
                }
            }
            Msg::Flush(ack) => {
                if let Some((f, _)) = file.as_mut() {
                    let _ = f.flush();
                }
                let _ = ack.send(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_keeps_three_files() {
        let dir = std::env::temp_dir().join(format!("verba-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("verba.log");
        for i in 0..5 {
            std::fs::write(&path, format!("{i}")).unwrap();
            rotate(&path);
        }
        assert!(!path.exists());
        assert_eq!(std::fs::read_to_string(rotated(&path, 1)).unwrap(), "4");
        assert_eq!(std::fs::read_to_string(rotated(&path, 3)).unwrap(), "2");
        assert!(!rotated(&path, 4).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
