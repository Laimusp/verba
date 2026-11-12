//! Обработка записи после остановки: частота устройства → 16 кГц, обрезка тишины (VAD), WAV.
//! Чистая логика без Windows: тестируется на синтетическом звуке.

pub const TARGET_RATE: u32 = 16_000;
/// кадр earshot: 256 отсчётов при 16 кГц = 16 мс
const VAD_FRAME: usize = 256;
const VAD_THRESHOLD: f32 = 0.5;
/// речь — это не меньше стольких кадров подряд (48 мс): щелчок клавиши речью не считается
const VAD_MIN_RUN: usize = 3;
/// поля вокруг речи: тихое начало слова и затухание конца VAD часто не видит
const PAD_BEFORE_MS: usize = 400;
const PAD_AFTER_MS: usize = 600;

/// Окно Блэкмана для ядра ресемплера.
fn blackman(x: f64) -> f64 {
    // x ∈ [-1, 1]
    let t = (x + 1.0) * 0.5;
    0.42 - 0.5 * (2.0 * std::f64::consts::PI * t).cos() + 0.08 * (4.0 * std::f64::consts::PI * t).cos()
}

/// Ядро фильтра таблицей: `TABLE_STEPS` точек на входной отсчёт, между ними — линейная
/// интерполяция. Без таблицы — три тригонометрии на отвод, 5 мин записи считались бы секунды.
const TABLE_STEPS: usize = 512;

/// Ресемплинг окном-sinc (фильтр нижних частот на 0,45 от меньшей частоты). Моно, запись целиком:
/// 5 мин 48 кГц → 16 кГц — порядка 0,3 с в release, без зависимостей.
pub fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() || from == 0 || to == 0 {
        return input.to_vec();
    }
    let ratio = f64::from(from) / f64::from(to);
    // частота среза в циклах на входной отсчёт
    let cutoff = 0.45 * f64::from(from.min(to)) / f64::from(from);
    const ZEROS: f64 = 8.0;
    let half = (ZEROS / (2.0 * cutoff)).ceil() as isize;
    let table_len = half as usize * TABLE_STEPS + 2;
    let table: Vec<f32> = (0..table_len)
        .map(|i| {
            let d = i as f64 / TABLE_STEPS as f64;
            if d >= half as f64 {
                return 0.0;
            }
            let x = 2.0 * cutoff * d;
            let sinc = if x.abs() < 1e-12 { 1.0 } else { (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x) };
            (sinc * blackman(d / half as f64)) as f32
        })
        .collect();
    let kernel = |d: f64| -> f32 {
        let pos = d.abs() * TABLE_STEPS as f64;
        let i = pos as usize;
        if i + 1 >= table_len {
            return 0.0;
        }
        let f = (pos - i as f64) as f32;
        table[i] + (table[i + 1] - table[i]) * f
    };
    let out_len = ((input.len() as f64) / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for n in 0..out_len {
        let t = n as f64 * ratio;
        let center = t.floor() as isize;
        let lo = (center - half + 1).max(0) as usize;
        let hi = ((center + half) as usize).min(input.len() - 1);
        let mut acc = 0.0f32;
        let mut norm = 0.0f32;
        for (k, &x) in input.iter().enumerate().take(hi + 1).skip(lo) {
            let h = kernel(t - k as f64);
            acc += x * h;
            norm += h;
        }
        // нормировка: постоянный сигнал проходит без изменения и на краях записи
        out.push(if norm.abs() > 1e-9 { acc / norm } else { 0.0 });
    }
    out
}

pub fn to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|s| {
            let v = if s.is_finite() { s.clamp(-1.0, 1.0) } else { 0.0 };
            (v * 32767.0).round() as i16
        })
        .collect()
}

/// Где в записи речь (по кадрам VAD).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Speech {
    /// первый отсчёт речи (16 кГц)
    pub start: usize,
    /// конец последнего кадра речи
    pub end: usize,
    /// сколько речи всего, мс
    pub speech_ms: u64,
}

/// Ищет речь в 16-кГц записи. None — речи нет.
pub fn find_speech(samples: &[i16]) -> Option<Speech> {
    let mut vad = earshot::Detector::default_boxed();
    let mut first: Option<usize> = None;
    let mut last_end = 0usize;
    let mut run = 0usize;
    let mut speech_frames = 0usize;
    for (i, frame) in samples.as_chunks::<VAD_FRAME>().0.iter().enumerate() {
        let score = vad.predict_i16(frame);
        if score >= VAD_THRESHOLD {
            run += 1;
            if run >= VAD_MIN_RUN {
                if run == VAD_MIN_RUN {
                    speech_frames += VAD_MIN_RUN;
                    let run_start = (i + 1 - VAD_MIN_RUN) * VAD_FRAME;
                    first.get_or_insert(run_start);
                } else {
                    speech_frames += 1;
                }
                last_end = (i + 1) * VAD_FRAME;
            }
        } else {
            run = 0;
        }
    }
    first.map(|start| Speech { start, end: last_end, speech_ms: (speech_frames * VAD_FRAME * 1000 / TARGET_RATE as usize) as u64 })
}

/// Обрезает тишину по краям с полями; середину (паузы между фразами) не трогает.
pub fn trim(samples: &[i16], speech: Speech) -> &[i16] {
    let per_ms = TARGET_RATE as usize / 1000;
    let start = speech.start.saturating_sub(PAD_BEFORE_MS * per_ms);
    let end = (speech.end + PAD_AFTER_MS * per_ms).min(samples.len());
    &samples[start..end.max(start)]
}

/// WAV: PCM 16 бит, моно. Его принимают все модели OpenRouter (research.md §OpenRouter).
pub fn wav(samples: &[i16], rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // размер блока fmt
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // каналов
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes()); // байт в секунду
    out.extend_from_slice(&2u16.to_le_bytes()); // байт на отсчёт
    out.extend_from_slice(&16u16.to_le_bytes()); // бит на отсчёт
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Итог обработки записи.
#[derive(Debug, Clone)]
pub struct Processed {
    pub wav: Vec<u8>,
    /// длина записи целиком, мс
    pub total_ms: u64,
    /// длина того, что уйдёт (после обрезки), мс
    pub sent_ms: u64,
    /// речь по VAD, мс; 0 — речи нет (если VAD включён)
    pub speech_ms: u64,
}

/// Моно-запись с частотой устройства → то, что уйдёт на распознавание.
pub fn process(mono: &[f32], rate: u32, trim_silence: bool) -> Processed {
    let total_ms = (mono.len() as u64 * 1000) / u64::from(rate.max(1));
    let pcm = to_i16(&resample(mono, rate, TARGET_RATE));
    let (body, speech_ms) = if trim_silence {
        match find_speech(&pcm) {
            Some(sp) => (trim(&pcm, sp), sp.speech_ms),
            None => (&pcm[..0], 0),
        }
    } else {
        (&pcm[..], total_ms)
    };
    let sent_ms = (body.len() as u64 * 1000) / u64::from(TARGET_RATE);
    Processed { wav: wav(body, TARGET_RATE), total_ms, sent_ms, speech_ms }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, rate: u32, ms: u32, amp: f32) -> Vec<f32> {
        let n = (rate as u64 * ms as u64 / 1000) as usize;
        (0..n).map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin()).collect()
    }

    #[test]
    fn wav_header_is_canonical() {
        let w = wav(&[0, 1, -1, i16::MAX, i16::MIN], 16_000);
        assert_eq!(w.len(), 44 + 10);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(w[4..8].try_into().unwrap()), 36 + 10);
        assert_eq!(&w[8..16], b"WAVEfmt ");
        assert_eq!(u16::from_le_bytes(w[20..22].try_into().unwrap()), 1, "PCM");
        assert_eq!(u16::from_le_bytes(w[22..24].try_into().unwrap()), 1, "моно");
        assert_eq!(u32::from_le_bytes(w[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u32::from_le_bytes(w[28..32].try_into().unwrap()), 32_000);
        assert_eq!(u16::from_le_bytes(w[34..36].try_into().unwrap()), 16);
        assert_eq!(&w[36..40], b"data");
        assert_eq!(u32::from_le_bytes(w[40..44].try_into().unwrap()), 10);
        assert_eq!(i16::from_le_bytes(w[50..52].try_into().unwrap()), i16::MAX);
    }

    #[test]
    fn empty_wav_is_valid() {
        let w = wav(&[], 16_000);
        assert_eq!(w.len(), 44);
        assert_eq!(u32::from_le_bytes(w[40..44].try_into().unwrap()), 0);
    }

    #[test]
    fn to_i16_clamps_and_kills_nan() {
        assert_eq!(to_i16(&[2.0, -2.0, f32::NAN, 0.5]), vec![32767, -32767, 0, 16384]);
    }

    #[test]
    fn resample_keeps_length_and_tone() {
        let input = sine(440.0, 48_000, 1000, 0.5);
        let out = resample(&input, 48_000, 16_000);
        assert_eq!(out.len(), 16_000);
        // амплитуда тона в середине сохраняется (±3 %)
        let peak = out[4000..12000].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!((peak - 0.5).abs() < 0.015, "пик {peak}");
    }

    #[test]
    fn resample_removes_tones_above_new_nyquist() {
        // 12 кГц не помещается в 16 кГц (предел 8 кГц) — должен уйти, а не отразиться в 4 кГц
        let input = sine(12_000.0, 48_000, 500, 0.5);
        let out = resample(&input, 48_000, 16_000);
        let peak = out[1000..7000].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak < 0.01, "остаток {peak}");
    }

    #[test]
    fn resample_44100_has_expected_length() {
        let input = vec![0.25f32; 44_100];
        let out = resample(&input, 44_100, 16_000);
        assert_eq!(out.len(), 16_000);
        assert!(out.iter().all(|s| (s - 0.25).abs() < 1e-3), "постоянный сигнал проходит без искажений");
    }

    #[test]
    fn silence_has_no_speech() {
        let p = process(&vec![0.0; 48_000], 48_000, true);
        assert_eq!(p.speech_ms, 0);
        assert_eq!(p.wav.len(), 44);
        assert_eq!(p.total_ms, 1000);
    }

    #[test]
    fn without_vad_everything_goes() {
        let p = process(&vec![0.0; 48_000], 48_000, false);
        assert_eq!(p.sent_ms, 1000);
        assert_eq!(p.wav.len(), 44 + 32_000);
    }

    #[test]
    fn trim_keeps_padding_and_middle() {
        let pcm = vec![0i16; 16_000 * 5];
        let sp = Speech { start: 16_000 * 2, end: 16_000 * 3, speech_ms: 1000 };
        let t = trim(&pcm, sp);
        assert_eq!(t.len(), 16_000 + 400 * 16 + 600 * 16);
        let edge = Speech { start: 100, end: pcm.len() - 10, speech_ms: 1 };
        assert_eq!(trim(&pcm, edge).len(), pcm.len(), "поля не выходят за края записи");
    }
}
