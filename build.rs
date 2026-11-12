//! Вшивает иконку `assets/verba.ico` в verba.exe: её показывают рабочий стол, панель задач и проводник.
//!
//! Ресурс (.res) собирается здесь же: пустая запись, по RT_ICON на картинку и RT_GROUP_ICON с их списком.
//! Компоновщик MSVC принимает .res на вход сам (документ «.Res Files as Linker Input»), поэтому rc.exe и
//! Windows SDK в PATH не нужны. Сам файл иконки собирает `tests/app_icon.rs` из знака в `src/icons.rs`.
use std::path::PathBuf;
use std::{env, fs};

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
/// en-US — язык ресурсов по умолчанию у rc.exe
const LANG: u16 = 0x0409;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/verba.ico");
    let msvc = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") || !msvc {
        println!("cargo:warning=иконка не вшита: .res принимает только компоновщик MSVC (x86_64-pc-windows-msvc)");
        return;
    }
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let Ok(ico) = fs::read(root.join("assets").join("verba.ico")) else {
        println!("cargo:warning=assets/verba.ico нет → exe без иконки; собрать: VERBA_WRITE_ICON=1 cargo test --test app_icon");
        return;
    };
    let res = res_from_ico(&ico).unwrap_or_else(|e| panic!("assets/verba.ico: {e}"));
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("verba.res");
    fs::write(&out, res).unwrap();
    println!("cargo:rustc-link-arg-bins={}", out.display());
}

/// Заголовок записи .res: тип и имя — числа (0xFFFF + номер), итого 32 байта.
fn header(out: &mut Vec<u8>, size: usize, kind: u16, id: u16, flags: u16, lang: u16) {
    out.extend((size as u32).to_le_bytes());
    out.extend(32u32.to_le_bytes());
    out.extend([0xFF, 0xFF]);
    out.extend(kind.to_le_bytes());
    out.extend([0xFF, 0xFF]);
    out.extend(id.to_le_bytes());
    out.extend(0u32.to_le_bytes()); // DataVersion
    out.extend(flags.to_le_bytes());
    out.extend(lang.to_le_bytes());
    out.extend(0u32.to_le_bytes()); // Version
    out.extend(0u32.to_le_bytes()); // Characteristics
}

fn data(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend(bytes);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

fn res_from_ico(ico: &[u8]) -> Result<Vec<u8>, String> {
    let u16_at = |i: usize| ico.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]])).ok_or("файл обрезан");
    let u32_at = |i: usize| ico.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])).ok_or("файл обрезан");
    if u16_at(0)? != 0 || u16_at(2)? != 1 {
        return Err("это не ICO".into());
    }
    let count = u16_at(4)?;
    if count == 0 {
        return Err("в ICO нет картинок".into());
    }
    let mut res = Vec::new();
    header(&mut res, 0, 0, 0, 0, 0); // любой .res начинается с пустой записи
    let mut group = Vec::new();
    group.extend(0u16.to_le_bytes());
    group.extend(1u16.to_le_bytes());
    group.extend(count.to_le_bytes());
    for k in 0..count as usize {
        let e = 6 + 16 * k;
        let entry = ico.get(e..e + 16).ok_or("каталог ICO обрезан")?;
        let (len, off) = (u32_at(e + 8)? as usize, u32_at(e + 12)? as usize);
        let image = ico.get(off..off + len).ok_or(format!("картинка {k} за концом файла"))?;
        let id = k as u16 + 1;
        header(&mut res, image.len(), RT_ICON, id, 0x1010, LANG);
        data(&mut res, image);
        // GRPICONDIRENTRY: первые 12 байт записи ICO и номер RT_ICON вместо смещения
        group.extend(&entry[..12]);
        group.extend(id.to_le_bytes());
    }
    header(&mut res, group.len(), RT_GROUP_ICON, 1, 0x1030, LANG);
    data(&mut res, &group);
    Ok(res)
}
