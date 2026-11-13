//! Очеловечивание текста перед вставкой (`paste.humanize`, по умолчанию выключено): знаки, которые
//! ставят модели распознавания и LLM, а человек с обычной клавиатуры почти не набирает, → клавиатурные.
//! Длинное тире → «-», «ёлочки» и типографские кавычки → `"`, многоточие одним знаком → «...»,
//! неразрывные и тонкие пробелы → обычный, невидимые знаки (нулевой ширины, мягкий перенос, метки
//! направления текста) — убираются. Почему именно эти знаки и источники — `docs/humanize.md`.
//!
//! Не трогает: буквы (ё, ́), цифры, ZWJ/ZWNJ (склеивают эмодзи-последовательности вроде 👨‍👩‍👧 и нужны
//! персидскому письму), селекторы вариантов (U+FE0F — цветной вид эмодзи), № и ₽ (есть на русской
//! раскладке), математику и единицы (×, ≈, ≤, °): их человек диктует как раз ради этих знаков.
//!
//! Чистая функция без состояния: зовёт контроллер прямо перед вставкой, текст в лог не пишется.

use std::borrow::Cow;

/// Что стало с одним знаком.
enum Fix {
    /// оставить как есть
    Keep,
    /// убрать (невидимый знак)
    Drop,
    /// заменить одним знаком
    Char(char),
    /// заменить строкой
    Str(&'static str),
    /// длинное тире: «-»; вплотную к слову — с пробелом (`dash`)
    LongDash,
}

fn fix(c: char) -> Fix {
    match c {
        // ---- пробелы: неразрывный, узкий неразрывный (его ставит GPT-5 вместо обычного),
        // en/em/тонкий/волосяной, цифровой, математический, идеографический. Только заменять:
        // удаление склеит слова
        '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => Fix::Char(' '),
        // разделители строки и абзаца → перенос, который человек ставит Enter
        '\u{2028}' | '\u{2029}' => Fix::Char('\n'),
        // ---- невидимые: пробел нулевой ширины, соединитель слов, BOM, мягкий перенос,
        // монгольский разделитель гласных, метки и вставки направления текста (и арабская),
        // невидимые математические знаки
        '\u{200B}' | '\u{2060}'..='\u{2064}' | '\u{FEFF}' | '\u{00AD}' | '\u{180E}' | '\u{200E}' | '\u{200F}' | '\u{061C}'
        | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' => Fix::Drop,
        // ---- тире: длинное, горизонтальная черта, двойное и тройное длинное
        '\u{2014}' | '\u{2015}' | '\u{2E3A}' | '\u{2E3B}' => Fix::LongDash,
        // короткое (диапазоны «1990–2000»; с пробелами — тире Word и Claude), цифровое, минус,
        // дефис и неразрывный дефис Юникода, малые и полноширинные дефисы
        '\u{2013}' | '\u{2012}' | '\u{2212}' | '\u{2010}' | '\u{2011}' | '\u{FE58}' | '\u{FE63}' | '\u{FF0D}' => Fix::Char('-'),
        // ---- кавычки: «ёлочки», „лапки“, “английские”, двойной штрих, полноширинная
        '\u{00AB}' | '\u{00BB}' | '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{2033}' | '\u{FF02}' => Fix::Char('"'),
        // одинарные ‘ ’ (апостроф в don’t), нижняя ‚, ‹ ›, штрих ′, буква-апостроф ʼ
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2039}' | '\u{203A}' | '\u{2032}' | '\u{FF07}' | '\u{02BC}' => {
            Fix::Char('\'')
        }
        // ---- многоточие одним знаком
        '\u{2026}' => Fix::Str("..."),
        _ => Fix::Keep,
    }
}

const OPEN: [char; 3] = ['(', '[', '{'];
const CLOSE: [char; 3] = [')', ']', '}'];
const PUNCT: [char; 7] = ['.', ',', ';', ':', '!', '?', '\u{2026}'];

fn is_quote(c: char) -> bool {
    matches!(c, '"' | '\'' | '\u{00AB}' | '\u{00BB}' | '\u{2018}'..='\u{201F}' | '\u{2039}' | '\u{203A}' | '\u{2032}' | '\u{2033}' | '\u{FF02}' | '\u{FF07}')
}

/// Ближайший знак исходника слева от `i`, который останется в тексте (невидимые пропускаем).
fn left(chars: &[char], i: usize) -> Option<usize> {
    (0..i).rev().find(|&j| !matches!(fix(chars[j]), Fix::Drop))
}

/// То же справа.
fn right(chars: &[char], i: usize) -> Option<usize> {
    (i + 1..chars.len()).find(|&j| !matches!(fix(chars[j]), Fix::Drop))
}

/// После знака `j` пробел перед тире не нужен: пробел, открывающая скобка или кавычка.
/// Кавычка открывает, если перед ней начало, пробел или открывающая скобка: „лапки“ закрывает «“».
fn opens(chars: &[char], j: usize) -> bool {
    let c = chars[j];
    if c.is_whitespace() || OPEN.contains(&c) {
        return true;
    }
    is_quote(c) && left(chars, j).is_none_or(|p| chars[p].is_whitespace() || OPEN.contains(&chars[p]))
}

/// Перед знаком `k` пробел после тире не нужен: пробел, закрывающая скобка, знак препинания или
/// закрывающая кавычка (после неё — конец, пробел, скобка или знак препинания).
fn closes(chars: &[char], k: usize) -> bool {
    let c = chars[k];
    let ends = |c: char| c.is_whitespace() || CLOSE.contains(&c) || PUNCT.contains(&c);
    if ends(c) {
        return true;
    }
    is_quote(c) && right(chars, k).is_none_or(|n| ends(chars[n]))
}

/// Длинное тире на месте `i`: между цифрами (`1941—1945`, `2—3 дня`) — «-» без пробелов, как пишут
/// диапазоны; иначе «-», а вплотную к слову, кавычке или скобке — с пробелом: `this—that` →
/// `this - that`, а не `this-that`, что читалось бы как слово через дефис.
fn dash(out: &mut String, chars: &[char], i: usize) {
    let l = left(chars, i);
    let r = right(chars, i);
    if l.zip(r).is_some_and(|(l, r)| chars[l].is_numeric() && chars[r].is_numeric()) {
        out.push('-');
        return;
    }
    if l.is_some_and(|l| !opens(chars, l)) {
        out.push(' ');
    }
    out.push('-');
    if r.is_some_and(|r| !closes(chars, r)) {
        out.push(' ');
    }
}

/// Текст с клавиатурными знаками и сколько знаков исходника заменено или убрано.
/// Нечего менять — исходная строка без копирования.
pub fn humanize(text: &str) -> (Cow<'_, str>, usize) {
    if text.chars().all(|c| matches!(fix(c), Fix::Keep)) {
        return (Cow::Borrowed(text), 0);
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 8);
    let mut changed = 0;
    for (i, &c) in chars.iter().enumerate() {
        match fix(c) {
            Fix::Keep => {
                out.push(c);
                continue;
            }
            Fix::Drop => {}
            Fix::Char(r) => out.push(r),
            Fix::Str(s) => out.push_str(s),
            Fix::LongDash => dash(&mut out, &chars, i),
        }
        changed += 1;
    }
    (Cow::Owned(out), changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> String {
        humanize(s).0.into_owned()
    }

    #[test]
    fn plain_text_is_not_copied() {
        let s = "Обычный текст - с дефисом, \"кавычками\" и ... точками. ё, №5, 2×2 ≈ 4, 5 °C, 100 ₽";
        let (out, n) = humanize(s);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(n, 0);
    }

    #[test]
    fn long_dash_becomes_keyboard_hyphen() {
        assert_eq!(h("Это — тест"), "Это - тест");
        assert_eq!(h("Москва\u{00A0}— столица"), "Москва - столица", "неразрывный пробел перед тире");
        assert_eq!(h("— Привет, — сказал он."), "- Привет, - сказал он.", "тире диалога");
        assert_eq!(h("Строка\n— реплика"), "Строка\n- реплика");
    }

    #[test]
    fn glued_long_dash_gets_spaces() {
        assert_eq!(h("this—that"), "this - that");
        assert_eq!(h("слово— слово"), "слово - слово");
        assert_eq!(h("—Привет"), "- Привет");
        assert_eq!(h("слово—\u{200B}слово"), "слово - слово", "невидимый знак между тире и словом");
        assert_eq!(h("(—)"), "(-)", "у скобок пробелы не нужны");
        assert_eq!(h("сказал,—и ушёл"), "сказал, - и ушёл");
        assert_eq!(h("слово—."), "слово -.");
    }

    #[test]
    fn long_dash_between_digits_is_a_range() {
        assert_eq!(h("1941—1945"), "1941-1945");
        assert_eq!(h("2—3 дня"), "2-3 дня");
        assert_eq!(h("1941 — 1945"), "1941 - 1945", "пробелы, поставленные моделью, остаются");
        assert_eq!(h("стр—5"), "стр - 5");
    }

    #[test]
    fn long_dash_next_to_quotes() {
        assert_eq!(h("«Война и мир»—роман"), "\"Война и мир\" - роман");
        assert_eq!(h("роман—«Война»"), "роман - \"Война\"");
        assert_eq!(h("«—Да»"), "\"- Да\"", "открывающая ёлочка");
        assert_eq!(h("„лапки“—текст"), "\"лапки\" - текст", "«“» здесь закрывает");
        assert_eq!(h("сказал—»"), "сказал -\"", "закрывающая ёлочка в конце");
    }

    #[test]
    fn short_dashes_and_minus() {
        assert_eq!(h("1990–2000"), "1990-2000");
        assert_eq!(h("слово – слово"), "слово - слово");
        assert_eq!(h("−5 °C"), "-5 °C");
        assert_eq!(h("кто\u{2011}то"), "кто-то");
    }

    #[test]
    fn quotes_and_apostrophes() {
        assert_eq!(h("«Ёлочки» и „лапки“"), "\"Ёлочки\" и \"лапки\"");
        assert_eq!(h("«Внешние „внутренние“»"), "\"Внешние \"внутренние\"\"");
        assert_eq!(h("“Hello,” she said"), "\"Hello,\" she said");
        assert_eq!(h("don’t ‘quote’ мʼята"), "don't 'quote' м'ята");
        assert_eq!(h("5′ 7″"), "5' 7\"");
    }

    #[test]
    fn ellipsis_spaces_and_breaks() {
        assert_eq!(h("Ну…"), "Ну...");
        assert_eq!(h("1\u{202F}000\u{00A0}₽"), "1 000 ₽");
        assert_eq!(h("а\u{2009}б\u{3000}в"), "а б в");
        assert_eq!(h("раз\u{2028}два\u{2029}три"), "раз\nдва\nтри");
    }

    #[test]
    fn invisible_marks_are_dropped() {
        assert_eq!(h("сло\u{00AD}во\u{200B} \u{FEFF}текст\u{200E}\u{061C}\u{2062}"), "слово текст");
    }

    #[test]
    fn emoji_sequences_survive() {
        let family = "семья 👨\u{200D}👩\u{200D}👧, ❤\u{FE0F} и флаг 🏴\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}";
        assert_eq!(h(family), family);
        assert_eq!(h("👨\u{200D}👩 — ок"), "👨\u{200D}👩 - ок");
    }

    #[test]
    fn count_is_per_source_char() {
        assert_eq!(humanize("a—b … «c»").1, 4);
        assert_eq!(humanize("").1, 0);
        assert_eq!(h(""), "");
        assert_eq!(h("—"), "-");
    }
}
