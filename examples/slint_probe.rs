//! Замер окна настроек на Slint: время открытия и private-память до, во время и после закрытия.
//! `cargo run --release --example slint_probe`
use std::time::{Duration, Instant};

use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::GetCurrentProcess;

slint::slint! {
    import { Button, LineEdit, Switch, ComboBox, ScrollView, Palette } from "std-widgets.slint";
    export component Probe inherits Window {
        title: "Verba — настройки";
        preferred-width: 720px;
        preferred-height: 560px;
        background: Palette.background;
        HorizontalLayout {
            padding: 16px;
            spacing: 16px;
            VerticalLayout {
                width: 160px;
                spacing: 4px;
                for t in ["Распознавание", "Хоткеи", "Вставка", "Микрофон и звук", "Общее"]: Rectangle {
                    height: 36px;
                    border-radius: 6px;
                    background: Palette.alternate-background;
                    Text { text: t; x: 12px; vertical-alignment: center; color: Palette.foreground; }
                }
                Rectangle {}
            }
            ScrollView {
                VerticalLayout {
                    spacing: 12px;
                    for i in 12: Rectangle {
                        height: 64px;
                        border-radius: 8px;
                        background: Palette.alternate-background;
                        HorizontalLayout {
                            padding: 12px;
                            spacing: 8px;
                            Text { text: "Ключ OpenRouter · строка " + i; vertical-alignment: center; color: Palette.foreground; }
                            LineEdit { placeholder-text: "sk-or-v1-…"; }
                            Switch { }
                            ComboBox { model: ["Микрофон по умолчанию", "Гарнитура"]; }
                            Button { text: "Проверить"; }
                        }
                    }
                }
            }
        }
    }
}

fn private_mb() -> f64 {
    let mut c = PROCESS_MEMORY_COUNTERS_EX::default();
    unsafe {
        let _ = GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        );
    }
    c.PrivateUsage as f64 / 1048576.0
}

thread_local! {
    static WIN: std::cell::RefCell<Option<Probe>> = const { std::cell::RefCell::new(None) };
}

fn open(n: u32) {
    let t = Instant::now();
    let p = Probe::new().unwrap();
    let created = t.elapsed();
    p.show().unwrap();
    let shown = t.elapsed();
    WIN.with(|w| *w.borrow_mut() = Some(p));
    slint::Timer::single_shot(Duration::ZERO, move || {
        println!("открытие #{n}: создание {:?}, show {:?}, до следующего цикла {:?}, private {:.1} МБ", created, shown, t.elapsed(), private_mb());
    });
}

fn close() {
    WIN.with(|w| {
        if let Some(p) = w.borrow_mut().take() {
            let _ = p.hide();
        }
    });
}

fn main() {
    println!("старт: private {:.1} МБ", private_mb());
    let mut at = 300u64;
    slint::Timer::single_shot(Duration::from_millis(at), || println!("цикл Slint без окон: private {:.1} МБ", private_mb()));
    for n in 1..=6u32 {
        at += 400;
        slint::Timer::single_shot(Duration::from_millis(at), move || open(n));
        at += 1500;
        slint::Timer::single_shot(Duration::from_millis(at), close);
        at += 1500;
        slint::Timer::single_shot(Duration::from_millis(at), move || println!("после закрытия #{n}: private {:.1} МБ", private_mb()));
    }
    at += 500;
    slint::Timer::single_shot(Duration::from_millis(at), || {
        let _ = slint::quit_event_loop();
    });
    slint::run_event_loop_until_quit().unwrap();
    println!("конец: private {:.1} МБ", private_mb());
}
