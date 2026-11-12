//! Иконка приложения `assets/verba.ico` — из того же знака, что значок трея (`src/icons.rs`).
//!
//!   cargo test --test app_icon                          сверить файл со знаком
//!   VERBA_WRITE_ICON=1 cargo test --test app_icon       пересобрать файл после правки знака
//!   VERBA_ICON_DUMP=<папка> cargo test --test app_icon  PNG всех размеров и состояний трея — посмотреть глазами
//!
//! Размеры — те, что берёт Windows: 16–64 для списков, панели задач и трея при любом масштабе, 256 для крупных значков.
//! До 64 px — 32-битный BMP (так их читает любой загрузчик), 256 — PNG.
use std::fs;
use std::path::{Path, PathBuf};

use verba::icons::{render, render_app, IconState};

const SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];

fn png(rgba: &[u8], size: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, size, size);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Best);
    enc.write_header().unwrap().write_image_data(rgba).unwrap();
    out
}

/// Картинка ICO в BMP: BITMAPINFOHEADER (высота ×2: цвет и маска), BGRA снизу вверх, маска AND (1 — прозрачно).
fn bmp(rgba: &[u8], size: u32) -> Vec<u8> {
    let mask_row = size.div_ceil(32) * 4;
    let mut out = Vec::new();
    out.extend(40u32.to_le_bytes());
    out.extend((size as i32).to_le_bytes());
    out.extend((2 * size as i32).to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(32u16.to_le_bytes());
    out.extend(0u32.to_le_bytes()); // BI_RGB
    out.extend((size * size * 4 + mask_row * size).to_le_bytes());
    out.extend([0u8; 16]);
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            out.extend([rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    for y in (0..size).rev() {
        let mut row = vec![0u8; mask_row as usize];
        for x in 0..size {
            if rgba[((y * size + x) * 4 + 3) as usize] == 0 {
                row[(x / 8) as usize] |= 0x80 >> (x % 8);
            }
        }
        out.extend(row);
    }
    out
}

fn ico() -> Vec<u8> {
    let images: Vec<(u32, Vec<u8>)> = SIZES
        .iter()
        .map(|&s| {
            let rgba = render_app(s);
            (s, if s >= 256 { png(&rgba, s) } else { bmp(&rgba, s) })
        })
        .collect();
    let mut out = Vec::new();
    out.extend(0u16.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend((images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (s, data) in &images {
        let side = if *s >= 256 { 0 } else { *s as u8 }; // 0 — это 256
        out.extend([side, side, 0, 0]);
        out.extend(1u16.to_le_bytes());
        out.extend(32u16.to_le_bytes());
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in &images {
        out.extend(data);
    }
    out
}

fn ico_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("assets").join("verba.ico")
}

#[test]
fn ico_file_matches_the_mark() {
    let fresh = ico();
    let path = ico_path();
    if std::env::var_os("VERBA_WRITE_ICON").is_some() {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &fresh).unwrap();
    }
    let saved = fs::read(&path).expect("assets/verba.ico нет → VERBA_WRITE_ICON=1 cargo test --test app_icon");
    assert!(saved == fresh, "assets/verba.ico не совпадает со знаком → VERBA_WRITE_ICON=1 cargo test --test app_icon");
}

#[test]
fn ico_directory_points_inside_the_file() {
    let data = ico();
    let n = u16::from_le_bytes([data[4], data[5]]) as usize;
    assert_eq!(n, SIZES.len());
    for k in 0..n {
        let e = &data[6 + 16 * k..6 + 16 * (k + 1)];
        let len = u32::from_le_bytes(e[8..12].try_into().unwrap()) as usize;
        let off = u32::from_le_bytes(e[12..16].try_into().unwrap()) as usize;
        assert!(off + len <= data.len(), "картинка {k} за концом файла");
        let side = if e[0] == 0 { 256 } else { e[0] as u32 };
        assert_eq!(side, SIZES[k]);
        if side == 256 {
            assert_eq!(&data[off..off + 8], b"\x89PNG\r\n\x1a\n");
        } else {
            assert_eq!(len as u32, 40 + side * side * 4 + side.div_ceil(32) * 4 * side);
        }
    }
}

/// Не проверка, а снимки для глаз: VERBA_ICON_DUMP=<папка>.
#[test]
fn dump_pngs_when_asked() {
    let Some(dir) = std::env::var_os("VERBA_ICON_DUMP") else { return };
    let dir = PathBuf::from(dir);
    fs::create_dir_all(&dir).unwrap();
    for s in SIZES {
        fs::write(dir.join(format!("app-{s}.png")), png(&render_app(s), s)).unwrap();
    }
    let states = [
        ("ready", IconState::Ready),
        ("recording", IconState::Recording),
        ("transcribing", IconState::Transcribing),
        ("paused", IconState::Paused),
        ("error", IconState::Error),
    ];
    for s in [16, 20, 24, 32] {
        for (name, st) in states {
            for (theme, dark) in [("dark", true), ("light", false)] {
                fs::write(dir.join(format!("tray-{name}-{s}-{theme}.png")), png(&render(st, s, dark), s)).unwrap();
            }
        }
    }
}
