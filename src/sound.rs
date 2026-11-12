//! Короткие звуки состояний: пауза и возврат, начало и конец записи, ошибка.
//! Тоны синтезируются при первом вызове; `PlaySound` асинхронный — вызывающий не ждёт.

use std::sync::OnceLock;

use windows::core::PCWSTR;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};

#[derive(Clone, Copy, Debug)]
pub enum Cue {
    Start,
    Stop,
    PauseOn,
    PauseOff,
    Error,
}

const RATE: u32 = 22_050;

/// Тон с мягкой огибающей (без щелчков): частоты по очереди, `ms` каждая, `gap_ms` между.
fn tones(freqs: &[f32], ms: u32, gap_ms: u32, volume: f32) -> Vec<u8> {
    let n = (RATE * ms / 1000) as usize;
    let gap = (RATE * gap_ms / 1000) as usize;
    let mut pcm: Vec<i16> = Vec::new();
    for (i, f) in freqs.iter().enumerate() {
        for k in 0..n {
            let t = k as f32 / RATE as f32;
            let env = (std::f32::consts::PI * k as f32 / n as f32).sin().powi(2);
            let s = (2.0 * std::f32::consts::PI * f * t).sin() * env * volume;
            pcm.push((s * 32767.0) as i16);
        }
        if i + 1 < freqs.len() {
            pcm.extend(std::iter::repeat_n(0, gap));
        }
    }
    crate::audio::dsp::wav(&pcm, RATE)
}

fn bank() -> &'static [Vec<u8>; 5] {
    static BANK: OnceLock<[Vec<u8>; 5]> = OnceLock::new();
    BANK.get_or_init(|| {
        [
            tones(&[880.0], 70, 0, 0.18),
            tones(&[660.0], 70, 0, 0.18),
            tones(&[660.0, 440.0], 60, 30, 0.22),
            tones(&[440.0, 660.0], 60, 30, 0.22),
            tones(&[330.0, 330.0], 90, 50, 0.25),
        ]
    })
}

pub fn play(cue: Cue) {
    let wav = &bank()[cue as usize];
    // SND_MEMORY: буфер живёт весь процесс (static) — асинхронное проигрывание читает его безопасно
    unsafe {
        let _ = PlaySoundW(PCWSTR(wav.as_ptr() as *const u16), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
    }
}
