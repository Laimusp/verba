//! Сочетание клавиш: строка конфига («ctrl+alt+p») ⇄ код виртуальной клавиши и маска модификаторов.
//! Чистая логика: хук получает сочетание упакованным в u32 и разбирает его без аллокаций.

pub const MOD_CTRL: u8 = 1;
pub const MOD_ALT: u8 = 2;
pub const MOD_SHIFT: u8 = 4;
pub const MOD_WIN: u8 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Binding {
    /// код виртуальной клавиши Windows (VK_*)
    pub vk: u16,
    /// MOD_* — какие модификаторы должны быть нажаты (ровно эти)
    pub mods: u8,
}

const VALID: u32 = 1 << 31;

impl Binding {
    pub fn pack(self) -> u32 {
        VALID | (u32::from(self.mods) << 16) | u32::from(self.vk)
    }

    pub fn unpack(v: u32) -> Option<Binding> {
        if v & VALID == 0 {
            return None;
        }
        Some(Binding { vk: (v & 0xFFFF) as u16, mods: ((v >> 16) & 0xFF) as u8 })
    }

    pub fn parse(s: &str) -> Result<Binding, String> {
        let parts: Vec<String> = s.split('+').map(|p| p.trim().to_ascii_lowercase()).collect();
        if parts.iter().any(|p| p.is_empty()) {
            return Err("пустая часть сочетания".into());
        }
        let (key, mods_part) = parts.split_last().ok_or("пустое сочетание")?;
        let mut mods = 0u8;
        for m in mods_part {
            let bit = match m.as_str() {
                "ctrl" | "control" => MOD_CTRL,
                "alt" => MOD_ALT,
                "shift" => MOD_SHIFT,
                "win" | "super" | "meta" => MOD_WIN,
                other => return Err(format!("«{other}» — не модификатор (ctrl, alt, shift, win)")),
            };
            if mods & bit != 0 {
                return Err(format!("модификатор «{m}» дважды"));
            }
            mods |= bit;
        }
        let vk = key_code(key).ok_or_else(|| format!("клавиша «{key}» неизвестна"))?;
        if let Some(own) = modifier_bit(vk) {
            if mods & own != 0 {
                return Err(format!("«{key}» уже есть среди модификаторов"));
            }
        }
        Ok(Binding { vk, mods })
    }

    /// Строка для config.json («ctrl+alt+p»): `parse` читает её обратно в то же сочетание.
    pub fn to_config(self) -> String {
        let mut out = String::new();
        for (bit, name) in [(MOD_CTRL, "ctrl"), (MOD_ALT, "alt"), (MOD_SHIFT, "shift"), (MOD_WIN, "win")] {
            if self.mods & bit != 0 {
                out.push_str(name);
                out.push('+');
            }
        }
        let key = match self.vk {
            0x41..=0x5A | 0x30..=0x39 => char::from(self.vk as u8).to_ascii_lowercase().to_string(),
            0x70..=0x87 => format!("f{}", self.vk - 0x70 + 1),
            0x60..=0x69 => format!("numpad{}", self.vk - 0x60),
            vk => NAMED.iter().find(|(_, v)| *v == vk).map(|(n, _)| n.to_string()).unwrap_or_else(|| format!("vk{vk:02x}")),
        };
        out.push_str(&key);
        out
    }

    /// «Ctrl+Alt+P» — для меню и сообщений.
    pub fn label(self) -> String {
        let mut out = String::new();
        for (bit, name) in [(MOD_CTRL, "Ctrl"), (MOD_ALT, "Alt"), (MOD_SHIFT, "Shift"), (MOD_WIN, "Win")] {
            if self.mods & bit != 0 {
                out.push_str(name);
                out.push('+');
            }
        }
        out.push_str(&key_label(self.vk));
        out
    }
}

/// Модификатор, к которому относится клавиша (левая и правая — один модификатор).
pub fn modifier_bit(vk: u16) -> Option<u8> {
    match vk {
        0x10 | 0xA0 | 0xA1 => Some(MOD_SHIFT),
        0x11 | 0xA2 | 0xA3 => Some(MOD_CTRL),
        0x12 | 0xA4 | 0xA5 => Some(MOD_ALT),
        0x5B | 0x5C => Some(MOD_WIN),
        _ => None,
    }
}

const NAMED: &[(&str, u16)] = &[
    ("space", 0x20),
    ("esc", 0x1B),
    ("escape", 0x1B),
    ("enter", 0x0D),
    ("return", 0x0D),
    ("tab", 0x09),
    ("backspace", 0x08),
    ("insert", 0x2D),
    ("ins", 0x2D),
    ("delete", 0x2E),
    ("del", 0x2E),
    ("home", 0x24),
    ("end", 0x23),
    ("pageup", 0x21),
    ("pgup", 0x21),
    ("pagedown", 0x22),
    ("pgdn", 0x22),
    ("up", 0x26),
    ("down", 0x28),
    ("left", 0x25),
    ("right", 0x27),
    ("pause", 0x13),
    ("capslock", 0x14),
    ("scrolllock", 0x91),
    ("printscreen", 0x2C),
    ("apps", 0x5D),
    ("menu", 0x5D),
    ("grave", 0xC0),
    ("`", 0xC0),
    ("minus", 0xBD),
    ("-", 0xBD),
    ("equals", 0xBB),
    ("=", 0xBB),
    ("comma", 0xBC),
    (",", 0xBC),
    ("period", 0xBE),
    (".", 0xBE),
    ("slash", 0xBF),
    ("/", 0xBF),
    ("semicolon", 0xBA),
    (";", 0xBA),
    ("quote", 0xDE),
    ("'", 0xDE),
    ("backslash", 0xDC),
    ("\\", 0xDC),
    ("lbracket", 0xDB),
    ("[", 0xDB),
    ("rbracket", 0xDD),
    ("]", 0xDD),
    ("lctrl", 0xA2),
    ("rctrl", 0xA3),
    ("lalt", 0xA4),
    ("ralt", 0xA5),
    ("lshift", 0xA0),
    ("rshift", 0xA1),
    ("lwin", 0x5B),
    ("rwin", 0x5C),
];

fn key_code(key: &str) -> Option<u16> {
    if let Some((_, vk)) = NAMED.iter().find(|(n, _)| *n == key) {
        return Some(*vk);
    }
    let b = key.as_bytes();
    if b.len() == 1 && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit()) {
        return Some(u16::from(b[0].to_ascii_uppercase()));
    }
    if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<u16>().ok()) {
        if (1..=24).contains(&n) {
            return Some(0x70 + n - 1);
        }
    }
    if let Some(n) = key.strip_prefix("numpad").and_then(|n| n.parse::<u16>().ok()) {
        if n <= 9 {
            return Some(0x60 + n);
        }
    }
    // «vk7c» или «vk0x7c» — любая клавиша по коду
    if let Some(hex) = key.strip_prefix("vk") {
        let hex = hex.strip_prefix("0x").unwrap_or(hex);
        if let Ok(v) = u16::from_str_radix(hex, 16) {
            if (1..=0xFE).contains(&v) {
                return Some(v);
            }
        }
    }
    None
}

fn key_label(vk: u16) -> String {
    match vk {
        0x41..=0x5A | 0x30..=0x39 => char::from(vk as u8).to_string(),
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),
        0x60..=0x69 => format!("Num{}", vk - 0x60),
        0x20 => "Space".into(),
        0x1B => "Esc".into(),
        0x0D => "Enter".into(),
        0x2D => "Insert".into(),
        0x13 => "Pause".into(),
        _ => NAMED
            .iter()
            .find(|(_, v)| *v == vk)
            .map(|(n, _)| {
                let mut c = n.chars();
                c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
            })
            .unwrap_or_else(|| format!("VK{vk:02X}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_forms() {
        assert_eq!(Binding::parse("ctrl+space").unwrap(), Binding { vk: 0x20, mods: MOD_CTRL });
        assert_eq!(Binding::parse("Ctrl + Alt + P").unwrap(), Binding { vk: 0x50, mods: MOD_CTRL | MOD_ALT });
        assert_eq!(Binding::parse("f13").unwrap(), Binding { vk: 0x7C, mods: 0 });
        assert_eq!(Binding::parse("esc").unwrap(), Binding { vk: 0x1B, mods: 0 });
        assert_eq!(Binding::parse("rctrl").unwrap(), Binding { vk: 0xA3, mods: 0 });
        assert_eq!(Binding::parse("win+shift+vk0x7c").unwrap(), Binding { vk: 0x7C, mods: MOD_WIN | MOD_SHIFT });
    }

    #[test]
    fn refuses_garbage() {
        assert!(Binding::parse("").is_err());
        assert!(Binding::parse("ctrl+").is_err());
        assert!(Binding::parse("hyper+a").is_err());
        assert!(Binding::parse("ctrl+ctrl+a").is_err());
        assert!(Binding::parse("ctrl+rctrl").is_err());
        assert!(Binding::parse("f25").is_err());
        assert!(Binding::parse("vk0").is_err());
    }

    #[test]
    fn pack_roundtrip() {
        let b = Binding::parse("ctrl+alt+shift+win+f12").unwrap();
        assert_eq!(Binding::unpack(b.pack()), Some(b));
        assert_eq!(Binding::unpack(0), None);
    }

    #[test]
    fn config_string_roundtrip() {
        for s in ["ctrl+space", "ctrl+alt+p", "win+shift+f13", "esc", "rctrl", "alt+numpad5", "ctrl+vk7f", "shift+`", "ctrl+9"] {
            let b = Binding::parse(s).unwrap();
            assert_eq!(Binding::parse(&b.to_config()).unwrap(), b, "{s} → {}", b.to_config());
        }
        assert_eq!(Binding { vk: 0x20, mods: MOD_CTRL | MOD_ALT }.to_config(), "ctrl+alt+space");
        assert_eq!(Binding { vk: 0x2D, mods: 0 }.to_config(), "insert");
    }

    #[test]
    fn labels() {
        assert_eq!(Binding::parse("ctrl+alt+p").unwrap().label(), "Ctrl+Alt+P");
        assert_eq!(Binding::parse("ctrl+space").unwrap().label(), "Ctrl+Space");
        assert_eq!(Binding::parse("f13").unwrap().label(), "F13");
    }
}
