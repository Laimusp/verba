//! Прокси: строка, как её дают сайты прокси → адрес, вид и логин. Перенос разбора из QED
//! (`qed\src\server\voice\proxy.ts` `parseProxy`, тестовые строки — оттуда же).
//!
//! Формы: `scheme://[user[:pass]@]host[:port]`, `host:port`, `user:pass@host:port`,
//! `host:port@user:pass`, `host:port:user:pass`, `user:pass:host:port`, части через пробел,
//! `|`, `;`, `,`; вид словом в начале или в конце; блок «IP: … Port: … Login: … Password: …».
//! Вид не назван → `kind = None`: Verba проверит HTTP, SOCKS5 и SOCKS4 сразу и запомнит ответивший.

use crate::config::ProxyKind;
use std::net::{Ipv4Addr, Ipv6Addr};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedProxy {
    pub kind: Option<ProxyKind>,
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub pass: Option<String>,
}

const ERR_FORM: &str =
    "строка не похожа на прокси → нужен адрес и порт, например 1.2.3.4:8080 или socks5://логин:пароль@1.2.3.4:1080";

fn scheme_kind(s: &str) -> Option<ProxyKind> {
    match s.to_ascii_lowercase().as_str() {
        "http" => Some(ProxyKind::Http),
        "https" => Some(ProxyKind::Https),
        "socks" | "socks5" | "socks5h" => Some(ProxyKind::Socks5),
        "socks4" | "socks4a" => Some(ProxyKind::Socks4),
        _ => None,
    }
}

fn default_port(k: ProxyKind) -> u16 {
    match k {
        ProxyKind::Http => 80,
        ProxyKind::Https => 443,
        ProxyKind::Socks4 | ProxyKind::Socks5 | ProxyKind::Auto => 1080,
    }
}

fn port_of(s: &str) -> Option<u16> {
    if s.is_empty() || s.len() > 5 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u16>().ok().filter(|p| *p >= 1)
}

fn is_domain(s: &str) -> bool {
    if s.is_empty() || s.len() > 253 || !s.contains('.') {
        return false;
    }
    let labels: Vec<&str> = s.split('.').collect();
    let ok_label = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            && !l.starts_with('-')
            && !l.ends_with('-')
    };
    let tld = labels.last().copied().unwrap_or("");
    labels.iter().all(|l| ok_label(l)) && tld.bytes().next().is_some_and(|b| b.is_ascii_alphabetic())
}

/// Адрес, который может быть у прокси: IPv4, IPv6 (в скобках или без), имя с точкой, localhost.
fn host_of(s: &str) -> Option<String> {
    let bare = s.strip_prefix('[').and_then(|x| x.strip_suffix(']')).unwrap_or(s);
    if bare.parse::<Ipv6Addr>().is_ok() {
        return Some(bare.to_string());
    }
    if s.parse::<Ipv4Addr>().is_ok() || is_domain(s) || s.eq_ignore_ascii_case("localhost") {
        return Some(s.to_string());
    }
    None
}

fn is_ipv4(s: &str) -> bool {
    s.parse::<Ipv4Addr>().is_ok()
}

/// `host:port` (IPv6 в скобках) → обе части.
fn host_port(s: &str) -> Option<(String, u16)> {
    if let Some(rest) = s.strip_prefix('[') {
        let (h, p) = rest.split_once("]:")?;
        return Some((host_of(&format!("[{h}]"))?, port_of(p)?));
    }
    let (h, p) = s.split_once(':')?;
    if p.contains(':') {
        return None;
    }
    Some((host_of(h)?, port_of(p)?))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    let hex = |c: u8| char::from(c).to_digit(16).map(|d| d as u8);
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

pub fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

type Body = (String, u16, Option<String>, Option<String>);

/// `user[:pass]` → логин и пароль; пустой логин — нет логина.
fn creds(s: &str, encoded: bool) -> (Option<String>, Option<String>) {
    let d = |v: &str| if encoded { percent_decode(v) } else { v.to_string() };
    let (user, pass) = match s.split_once(':') {
        Some((u, p)) => (u, Some(p)),
        None => (s, None),
    };
    if user.is_empty() {
        return (None, None);
    }
    match pass {
        Some(p) if !p.is_empty() => (Some(d(user)), Some(d(p))),
        _ => (Some(d(user)), None),
    }
}

/// Части строки без схемы по «:», `[ipv6]` — целиком.
fn colon_parts(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_bracket = false;
    for ch in s.chars() {
        if ch == '[' {
            in_bracket = true;
        }
        if ch == ']' {
            in_bracket = false;
        }
        if ch == ':' && !in_bracket {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(ch);
        }
    }
    out.push(cur);
    out
}

fn from_parts(parts: &[String]) -> Option<Body> {
    match parts.len() {
        2 => Some((host_of(&parts[0])?, port_of(&parts[1])?, None, None)),
        3 => {
            let host = host_of(&parts[0])?;
            let port = port_of(&parts[1])?;
            (!parts[2].is_empty()).then(|| (host, port, Some(parts[2].clone()), None))
        }
        n if n >= 4 => {
            let (a, b) = (&parts[0], &parts[1]);
            let (c, d) = (&parts[n - 2], &parts[n - 1]);
            // host:port:user:pass — пароль может содержать «:»
            let host_first = match (host_of(a), port_of(b)) {
                (Some(h), Some(p)) => Some((h, p, parts[2].clone(), parts[3..].join(":"))),
                _ => None,
            };
            // user:pass:host:port
            let host_last = if n == 4 {
                match (host_of(c), port_of(d)) {
                    (Some(h), Some(p)) => Some((h, p, a.clone(), b.clone())),
                    _ => None,
                }
            } else {
                None
            };
            let pick = match (host_first, host_last) {
                (Some(f), Some(l)) => {
                    if is_ipv4(c) && !is_ipv4(a) {
                        l
                    } else {
                        f
                    }
                }
                (Some(f), None) => f,
                (None, Some(l)) => l,
                (None, None) => return None,
            };
            let (h, p, user, pass) = pick;
            if user.is_empty() {
                return Some((h, p, None, None));
            }
            Some((h, p, Some(user), (!pass.is_empty()).then_some(pass)))
        }
        _ => None,
    }
}

fn body(s: &str, encoded: bool) -> Option<Body> {
    if let Some(at) = s.rfind('@') {
        if at > 0 {
            let (left, right) = (&s[..at], &s[at + 1..]);
            if let Some((h, p)) = host_port(right) {
                let (u, pw) = creds(left, encoded);
                return Some((h, p, u, pw));
            }
            // host:port@user:pass
            let first = s.find('@').unwrap_or(at);
            if let Some((h, p)) = host_port(&s[..first]) {
                let (u, pw) = creds(&s[first + 1..], encoded);
                return Some((h, p, u, pw));
            }
            return None;
        }
    }
    if let Some(b) = from_parts(&colon_parts(s)) {
        return Some(b);
    }
    let tokens: Vec<&str> = s.split(|c: char| c.is_whitespace() || "|;,".contains(c)).filter(|t| !t.is_empty()).collect();
    if tokens.len() >= 2 {
        let flat: Vec<String> =
            tokens.iter().flat_map(|t| if t.starts_with('[') { vec![t.to_string()] } else { colon_parts(t) }).collect();
        return from_parts(&flat);
    }
    None
}

fn label_key(label: &str) -> Option<&'static str> {
    let l = label.trim().to_lowercase();
    let l = l.as_str();
    match l {
        "ip" | "ip address" | "ip-address" | "ip адрес" | "ip-адрес" | "host" | "hostname" | "server" | "address" | "адрес"
        | "хост" | "сервер" => Some("host"),
        "port" | "порт" => Some("port"),
        "login" | "user" | "username" | "логин" | "пользователь" | "имя пользователя" => Some("user"),
        "pass" | "password" | "пароль" => Some("pass"),
        "type" | "protocol" | "proto" | "тип" | "протокол" => Some("type"),
        _ => None,
    }
}

/// «IP: 1.2.3.4 / Port: 8080 / Login: … / Password: …» — строками или в одну строку через «=».
fn labelled(text: &str) -> Option<ParsedProxy> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.contains('=') {
            for tok in line.split_whitespace() {
                if let Some((k, v)) = tok.split_once('=') {
                    pairs.push((k.to_string(), v.to_string()));
                }
            }
        } else if let Some((k, v)) = line.split_once(':') {
            if let Some(first) = v.split_whitespace().next() {
                pairs.push((k.to_string(), first.to_string()));
            }
        }
    }
    let get = |key: &str| {
        pairs.iter().find(|(k, _)| label_key(k) == Some(key)).map(|(_, v)| v.clone())
    };
    let host = host_of(&get("host")?)?;
    let port = port_of(&get("port")?)?;
    let kind = get("type").and_then(|t| scheme_kind(&t.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>()));
    let user = get("user");
    let pass = if user.is_some() { get("pass") } else { None };
    Some(ParsedProxy { kind, host, port, user, pass })
}

fn ok(kind: Option<ProxyKind>, b: Body) -> ParsedProxy {
    ParsedProxy { kind, host: b.0, port: b.1, user: b.2, pass: b.3 }
}

/// Строка → прокси или слова, что не так.
pub fn parse(text: &str) -> Result<ParsedProxy, String> {
    let raw = text.trim_start_matches('\u{feff}').trim();
    if raw.is_empty() {
        return Err("строка пустая → скопируй адрес прокси".into());
    }
    if raw.contains('\n') {
        if let Some(l) = labelled(raw) {
            return Ok(l);
        }
    }
    let line = raw.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let line = line.trim_matches(|c| c == '\'' || c == '"' || c == '`');
    if line.contains('=') || line.contains(": ") || line.contains(" :") {
        if let Some(l) = labelled(line) {
            return Ok(l);
        }
    }
    if let Some((scheme, rest)) = line.split_once("://") {
        let kind = scheme_kind(scheme)
            .ok_or_else(|| format!("вид прокси «{scheme}» не знаю → нужен http, https, socks4 или socks5"))?;
        // путь после адреса отбрасываем; логин оставляем целиком (в пароле бывает «/»)
        let at = rest.rfind('@').map(|i| i + 1).unwrap_or(0);
        let addr = &rest[at..];
        let cut = addr.find(['/', '?', '#']).unwrap_or(addr.len());
        let rest = format!("{}{}", &rest[..at], &addr[..cut]);
        if let Some(b) = body(&rest, true) {
            return Ok(ok(Some(kind), b));
        }
        if let Some(h) = host_of(&rest) {
            return Ok(ParsedProxy { kind: Some(kind), host: h, port: default_port(kind), user: None, pass: None });
        }
        return Err(ERR_FORM.into());
    }
    // «socks5 1.2.3.4:1080», «socks5:1.2.3.4:1080:user:pass»
    if let Some(i) = line.find([' ', '\t', ':']) {
        let (word, rest) = (&line[..i], line[i + 1..].trim());
        if let Some(kind) = scheme_kind(word) {
            if let Some(b) = body(rest, false) {
                return Ok(ok(Some(kind), b));
            }
        }
    }
    // «1.2.3.4:1080 socks5», «1.2.3.4:8080:user:pass:http»
    if let Some(i) = line.rfind([' ', '\t', ':']) {
        let (rest, word) = (line[..i].trim(), &line[i + 1..]);
        if let Some(kind) = scheme_kind(word) {
            if let Some(b) = body(rest, false) {
                return Ok(ok(Some(kind), b));
            }
        }
    }
    body(line, false).map(|b| ok(None, b)).ok_or_else(|| ERR_FORM.into())
}

/// Адрес прокси для reqwest: `socks5h://user:pass@host:port`. Для SOCKS5 имя сайта резолвит
/// прокси (как в QED): hosts владельца подменяет openrouter.ai на свой VPS, а прокси должен
/// ходить к настоящему адресу.
pub fn proxy_url(kind: ProxyKind, host: &str, port: u16, user: Option<&str>, pass: Option<&str>) -> String {
    let scheme = match kind {
        ProxyKind::Http | ProxyKind::Auto => "http",
        ProxyKind::Https => "https",
        ProxyKind::Socks4 => "socks4a",
        ProxyKind::Socks5 => "socks5h",
    };
    let host = if host.contains(':') { format!("[{host}]") } else { host.to_string() };
    let login = match (user, pass) {
        (Some(u), Some(p)) => format!("{}:{}@", percent_encode(u), percent_encode(p)),
        (Some(u), None) => format!("{}@", percent_encode(u)),
        _ => String::new(),
    };
    format!("{scheme}://{login}{host}:{port}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ProxyKind::*;

    fn p(kind: Option<ProxyKind>, host: &str, port: u16, user: Option<&str>, pass: Option<&str>) -> ParsedProxy {
        ParsedProxy { kind, host: host.into(), port, user: user.map(Into::into), pass: pass.map(Into::into) }
    }

    #[test]
    fn forms_of_proxy_sites() {
        let cases: Vec<(&str, ParsedProxy)> = vec![
            ("1.2.3.4:8080", p(None, "1.2.3.4", 8080, None, None)),
            ("  1.2.3.4:8080\n", p(None, "1.2.3.4", 8080, None, None)),
            ("proxy.example.com:3128", p(None, "proxy.example.com", 3128, None, None)),
            ("[2001:db8::1]:1080", p(None, "2001:db8::1", 1080, None, None)),
            ("http://1.2.3.4:8080", p(Some(Http), "1.2.3.4", 8080, None, None)),
            ("https://proxy.example.com:443/", p(Some(Https), "proxy.example.com", 443, None, None)),
            ("socks5://user:pass@1.2.3.4:1080", p(Some(Socks5), "1.2.3.4", 1080, Some("user"), Some("pass"))),
            ("socks5h://user:p%40ss@1.2.3.4:1080", p(Some(Socks5), "1.2.3.4", 1080, Some("user"), Some("p@ss"))),
            ("SOCKS4://1.2.3.4:1080", p(Some(Socks4), "1.2.3.4", 1080, None, None)),
            ("socks://1.2.3.4:1080", p(Some(Socks5), "1.2.3.4", 1080, None, None)),
            ("http://proxy.example.com", p(Some(Http), "proxy.example.com", 80, None, None)),
            ("http://user:pa/ss@1.2.3.4:8080", p(Some(Http), "1.2.3.4", 8080, Some("user"), Some("pa/ss"))),
            ("user:pass@1.2.3.4:8080", p(None, "1.2.3.4", 8080, Some("user"), Some("pass"))),
            ("us@er:p@ss@1.2.3.4:8080", p(None, "1.2.3.4", 8080, Some("us@er"), Some("p@ss"))),
            ("1.2.3.4:8080@user:pass", p(None, "1.2.3.4", 8080, Some("user"), Some("pass"))),
            ("1.2.3.4:8080:user:pass", p(None, "1.2.3.4", 8080, Some("user"), Some("pass"))),
            ("1.2.3.4:8080:user:pa:ss", p(None, "1.2.3.4", 8080, Some("user"), Some("pa:ss"))),
            ("user:pass:1.2.3.4:8080", p(None, "1.2.3.4", 8080, Some("user"), Some("pass"))),
            ("proxy.example.com:3128:login:secret", p(None, "proxy.example.com", 3128, Some("login"), Some("secret"))),
            ("1.2.3.4:1080:userid", p(None, "1.2.3.4", 1080, Some("userid"), None)),
            ("1.2.3.4 8080 user pass", p(None, "1.2.3.4", 8080, Some("user"), Some("pass"))),
            ("1.2.3.4:8080 user:pass", p(None, "1.2.3.4", 8080, Some("user"), Some("pass"))),
            ("1.2.3.4\t8080\tuser\tpass", p(None, "1.2.3.4", 8080, Some("user"), Some("pass"))),
            ("1.2.3.4|8080|user|pass", p(None, "1.2.3.4", 8080, Some("user"), Some("pass"))),
            ("1.2.3.4;8080;user;pass", p(None, "1.2.3.4", 8080, Some("user"), Some("pass"))),
            ("socks5 1.2.3.4:1080", p(Some(Socks5), "1.2.3.4", 1080, None, None)),
            ("socks5:1.2.3.4:1080:user:pass", p(Some(Socks5), "1.2.3.4", 1080, Some("user"), Some("pass"))),
            ("1.2.3.4:1080 socks5", p(Some(Socks5), "1.2.3.4", 1080, None, None)),
            ("1.2.3.4:8080:user:pass:http", p(Some(Http), "1.2.3.4", 8080, Some("user"), Some("pass"))),
            ("\"1.2.3.4:8080\"", p(None, "1.2.3.4", 8080, None, None)),
            ("1.2.3.4:8080\n5.6.7.8:3128", p(None, "1.2.3.4", 8080, None, None)),
            (
                "IP: 1.2.3.4\nPort: 8000\nLogin: user\nPassword: pass\nType: SOCKS5",
                p(Some(Socks5), "1.2.3.4", 8000, Some("user"), Some("pass")),
            ),
            ("IP-адрес: 1.2.3.4\nПорт: 8000\nЛогин: user\nПароль: pass", p(None, "1.2.3.4", 8000, Some("user"), Some("pass"))),
            (
                "Host=1.2.3.4 Port=3128 Username=u Password=p Protocol=http",
                p(Some(Http), "1.2.3.4", 3128, Some("u"), Some("p")),
            ),
        ];
        for (text, want) in cases {
            assert_eq!(parse(text).unwrap_or_else(|e| panic!("{text:?}: {e}")), want, "{text:?}");
        }
    }

    #[test]
    fn says_what_is_wrong() {
        assert!(parse("").unwrap_err().contains("пустая"));
        assert!(parse("ftp://1.2.3.4:21").unwrap_err().contains("ftp"));
        assert!(parse("hello").unwrap_err().contains("не похожа"));
        assert!(parse("1.2.3.4:99999").is_err());
        assert!(parse("1.2.3.4").is_err());
    }

    #[test]
    fn url_encodes_login_and_lets_proxy_resolve() {
        assert_eq!(proxy_url(Socks5, "1.2.3.4", 1080, Some("u"), Some("p@ss:1")), "socks5h://u:p%40ss%3A1@1.2.3.4:1080");
        assert_eq!(proxy_url(Http, "2001:db8::1", 8080, None, None), "http://[2001:db8::1]:8080");
        assert_eq!(proxy_url(Socks4, "h.example.com", 1080, Some("id"), None), "socks4a://id@h.example.com:1080");
    }
}
