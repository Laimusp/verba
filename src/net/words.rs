//! Отказы и сбои сети — словами «что не так → что сделать» (CLAUDE.md §4).
//! Тексты отказов — по образцу QED `service.ts` `refusalWords`, действия — под трей Verba.

/// Свои слова OpenRouter из ответа: `{error: {message}}` или `{message}`.
pub fn or_message(json: Option<&serde_json::Value>) -> String {
    let Some(j) = json else { return String::new() };
    let pick = |v: &serde_json::Value| v.as_str().map(str::to_string);
    j.get("error")
        .and_then(|e| e.get("message").and_then(pick).or_else(|| pick(e)))
        .or_else(|| j.get("message").and_then(pick))
        .map(|s| s.chars().take(200).collect())
        .unwrap_or_default()
}

pub fn no_key() -> String {
    "нет ключа OpenRouter → вставь его в настройках Verba (значок в трее); ключ — на openrouter.ai/settings/keys".into()
}

pub fn refusal(status: u16, said: &str, model: &str) -> String {
    let tail = if said.is_empty() { String::new() } else { format!(": {said}") };
    match status {
        401 | 403 => "OpenRouter не принял ключ → замени его в настройках Verba (значок в трее)".into(),
        402 => "на счету OpenRouter не хватает денег → пополни баланс на openrouter.ai/settings/credits".into(),
        404 => format!("у OpenRouter нет модели {model} → выбери другую в настройках Verba{tail}"),
        408 | 504 => "модель не успела распознать запись → «вставить последнее» отправит её снова".into(),
        413 => "запись слишком большая для OpenRouter → запиши короче".into(),
        429 => "OpenRouter просит подождать → «вставить последнее» через минуту".into(),
        s if s >= 500 => format!("OpenRouter не справился ({s}){tail} → «вставить последнее» отправит запись снова"),
        s => format!("OpenRouter отказал ({s}){tail}"),
    }
}

/// Куда шла запись: от этого зависят слова о сбое сети.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    OpenRouter,
    /// chatgpt.com — движок «ChatGPT по входу Codex»
    ChatGpt,
}

/// Сеть не довела запрос до ответа (все отправки оборвались или молчали).
pub fn unreachable(why: &str, via_proxy: bool) -> String {
    unreachable_at(Service::OpenRouter, why, via_proxy)
}

pub fn unreachable_at(service: Service, why: &str, via_proxy: bool) -> String {
    let (name, check) = match (service, via_proxy) {
        (Service::OpenRouter, true) => ("OpenRouter", "проверь интернет и прокси"),
        (Service::OpenRouter, false) => ("OpenRouter", "проверь интернет"),
        (Service::ChatGpt, true) => ("chatgpt.com", "проверь интернет и прокси"),
        // путь до chatgpt.com из России — только VPN или прокси на уровне системы
        (Service::ChatGpt, false) => ("chatgpt.com", "проверь интернет и VPN"),
    };
    format!("{name} недоступен: {why} → {check}; запись сохранена — «вставить последнее» отправит её снова")
}

pub fn timeout(secs: u64, via_proxy: bool) -> String {
    unreachable(&format!("нет ответа за {secs} с"), via_proxy)
}

/// Причина сбоя reqwest коротко: вид сбоя и самая глубокая причина из цепочки.
pub fn reqwest_why(e: &reqwest::Error) -> String {
    let kind = if e.is_connect() {
        "не подключиться"
    } else if e.is_timeout() {
        "не ответил вовремя"
    } else if e.is_body() || e.is_decode() {
        "ответ оборвался"
    } else {
        "соединение оборвалось"
    };
    let mut root: &dyn std::error::Error = e;
    while let Some(next) = root.source() {
        root = next;
    }
    let detail = root.to_string();
    if detail.is_empty() || detail == e.to_string() && !e.is_connect() {
        kind.to_string()
    } else {
        format!("{kind} ({detail})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn refusals_say_what_to_do() {
        for s in [401u16, 402, 404, 408, 413, 429, 500, 503] {
            let w = refusal(s, "", "m/x");
            assert!(w.contains('→'), "{s}: {w}");
        }
        assert!(refusal(404, "no such model", "m/x").contains("m/x"));
    }

    #[test]
    fn message_from_openrouter_json() {
        assert_eq!(or_message(Some(&json!({"error": {"message": "bad", "code": 400}}))), "bad");
        assert_eq!(or_message(Some(&json!({"error": "plain"}))), "plain");
        assert_eq!(or_message(Some(&json!({"message": "m"}))), "m");
        assert_eq!(or_message(None), "");
    }
}
