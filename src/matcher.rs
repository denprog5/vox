//! Распознавание русского мата в тексте, который выдала модель распознавания речи.
//!
//! Подход: каждое слово нормализуется (нижний регистр, `ё` → `е`, только буквы),
//! затем проверяется по корням. Корни, которые часто встречаются внутри обычных
//! слов (`ху`, `еб`), матчатся только в начале слова после известной приставки —
//! так «тихую», «хлеб», «небо», «психую», «страхуем» не считаются.

/// Приставки, после которых корень `ху[йеяюи]` считается матом (`похуй`, `нахуя`, `охуеть`).
const HU_PREFIXES: &[&str] = &[
    "", "а", "о", "об", "от", "по", "на", "за", "до", "ни", "не", "вы", "у", "с", "раз", "рас",
    "при", "пере", "недо", "под", "про",
];

/// Приставки, после которых корень `еб` считается матом (`заебал`, `съебался`, `долбоеб`).
/// Без голых `с`/`в`/`об`/`не`: иначе «себя», «небо», «тебя» и т.п.
const EB_PREFIXES: &[&str] = &[
    "",
    "за",
    "вы",
    "у",
    "про",
    "на",
    "от",
    "отъ",
    "до",
    "при",
    "по",
    "пере",
    "недо",
    "съ",
    "въ",
    "объ",
    "подъ",
    "разъ",
    "взъ",
    "изъ",
    "долбо",
    // так GigaAM и Zipformer часто пишут «долбоёб» на слух
    "долба",
    "мудо",
];

/// Подстроки, которые не встречаются в обычных словах.
const ANYWHERE: &[&str] = &["пизд", "залуп", "гандон", "гондон", "мандав"];

/// Начала слов («пидар» не в ANYWHERE из-за «скипидар»).
const STARTS: &[&str] = &[
    "бля",
    "мудак",
    "мудач",
    "мудил",
    "мудозвон",
    "пидор",
    "пидар",
    "пидр",
];

/// Точные словоформы (корни, которые иначе ловят нормальные слова: «мандарин», «сукно»).
const EXACT: &[&str] = &[
    "хули",
    "сука",
    "суки",
    "суке",
    "суку",
    "сукой",
    "сучка",
    "сучки",
    "сучку",
    "сучке",
    "сучара",
    "сукин",
    "сукина",
    "сукины",
    "манда",
    "манды",
    "манде",
    "манду",
    "мандой",
];

/// Обычные слова, которые формально попадают под правила выше.
/// `ебе`/`ебя`/`ебрр` — обрывки «тебе»/«себя», встречавшиеся в выводе распознавателя.
const NOT_MAT: &[&str] = &["ебонит", "ебонита", "ебе", "ебя", "ебрр"];

/// Начала обычных слов: «бляха», «бляшка» и их формы.
const NOT_MAT_STARTS: &[&str] = &["блях", "бляш"];

#[derive(Debug, Clone, Default)]
pub struct Matcher {
    /// Пользовательские слова: точное совпадение или префикс, если запись оканчивается на `*`.
    extra_exact: Vec<String>,
    extra_prefix: Vec<String>,
    ignore: Vec<String>,
}

impl Matcher {
    pub fn new(extra_words: &[String], ignore_words: &[String]) -> Self {
        let mut m = Matcher::default();
        for w in extra_words {
            match w.strip_suffix('*') {
                Some(p) => push_norm(&mut m.extra_prefix, p),
                None => push_norm(&mut m.extra_exact, w),
            }
        }
        for w in ignore_words {
            push_norm(&mut m.ignore, w);
        }
        m
    }

    /// Сколько матерных слов в тексте.
    pub fn count(&self, text: &str) -> u32 {
        let mut buf = String::new();
        let mut n = 0;
        for raw in text.split_whitespace() {
            normalize_into(raw, &mut buf);
            if !buf.is_empty() && self.is_mat(&buf) {
                n += 1;
            }
        }
        n
    }

    /// `word` уже нормализовано.
    fn is_mat(&self, word: &str) -> bool {
        if self.ignore.iter().any(|w| w == word) {
            return false;
        }
        if self.extra_exact.iter().any(|w| w == word)
            || self
                .extra_prefix
                .iter()
                .any(|p| word.starts_with(p.as_str()))
        {
            return true;
        }
        is_builtin_mat(word)
    }
}

fn is_builtin_mat(w: &str) -> bool {
    if NOT_MAT.contains(&w) || NOT_MAT_STARTS.iter().any(|s| w.starts_with(s)) {
        return false;
    }
    if EXACT.contains(&w)
        || ANYWHERE.iter().any(|s| w.contains(s))
        || STARTS.iter().any(|s| w.starts_with(s))
    {
        return true;
    }
    let hu = HU_PREFIXES.iter().any(|p| {
        w.strip_prefix(p)
            .and_then(|r| r.strip_prefix("ху"))
            .and_then(|r| r.chars().next())
            .is_some_and(|c| matches!(c, 'й' | 'е' | 'я' | 'ю' | 'и'))
    });
    hu || EB_PREFIXES
        .iter()
        .any(|p| w.strip_prefix(p).is_some_and(|r| r.starts_with("еб")))
}

fn push_norm(dst: &mut Vec<String>, w: &str) {
    let mut s = String::new();
    normalize_into(w, &mut s);
    if !s.is_empty() {
        dst.push(s);
    }
}

fn normalize_into(raw: &str, out: &mut String) {
    out.clear();
    for c in raw.chars() {
        for lc in c.to_lowercase() {
            match lc {
                'ё' => out.push('е'),
                c if c.is_alphabetic() => out.push(c),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m() -> Matcher {
        Matcher::default()
    }

    #[test]
    fn matcher_counts_common_mat() {
        let words = [
            "хуй",
            "хуйня",
            "хуево",
            "хуёвый",
            "хуярить",
            "нахуй",
            "нахуя",
            "похуй",
            "похуист",
            "охуеть",
            "ахуенно",
            "нихуя",
            "дохуя",
            "схуяли",
            "хули",
            "пизда",
            "пиздец",
            "спиздил",
            "распиздяй",
            "ебать",
            "ёбаный",
            "заебал",
            "заебись",
            "выебываться",
            "уебок",
            "проебал",
            "съебался",
            "отъебись",
            "долбоёб",
            "долбаеб",
            "ебло",
            "блять",
            "блядь",
            "бля",
            "сука",
            "сучка",
            "мудак",
            "мудила",
            "пидорас",
            "пидр",
            "залупа",
            "гандон",
            "манда",
            "мандавошка",
            "наебнулся",
            "поебать",
        ];
        for w in words {
            assert_eq!(m().count(w), 1, "должно считаться: {w}");
        }
    }

    #[test]
    fn matcher_ignores_normal_words() {
        let words = [
            "тихую",
            "плохую",
            "сухую",
            "глухую",
            "психую",
            "страхуем",
            "застрахуешь",
            "худой",
            "хулиган",
            "хуже",
            "ухудшать",
            "похудеть",
            "хлеб",
            "небо",
            "себя",
            "тебе",
            "ребенок",
            "требовать",
            "колебаться",
            "учеба",
            "погреб",
            "оскорблять",
            "употреблять",
            "истреблять",
            "мудрый",
            "премудрость",
            "мандарин",
            "сукно",
            "бляха",
            "ебонит",
            "блин",
            "гребаный",
            "скипидар",
            "бляшки",
            "бляхами",
            "команда",
            "мандат",
            "шлюха",
            "требую",
            "колеблется",
        ];
        for w in words {
            assert_eq!(m().count(w), 0, "не должно считаться: {w}");
        }
    }

    #[test]
    fn matcher_counts_every_word_in_phrase() {
        assert_eq!(m().count("ну блять опять этот пиздец нахуй"), 3);
        assert_eq!(m().count("Сука, ЁБАНЫЙ   стыд!"), 2);
        assert_eq!(m().count(""), 0);
    }

    #[test]
    fn matcher_respects_user_lists() {
        let m = Matcher::new(&["херня".into(), "шлюх*".into()], &["сука".into()]);
        assert_eq!(m.count("херня"), 1);
        assert_eq!(m.count("шлюхи"), 1);
        assert_eq!(m.count("сука"), 0);
        assert_eq!(m.count("хер"), 0);
    }

    /// Прогон матчера по словарю модели: `VOCAB=путь cargo test vocab_scan -- --ignored --nocapture`.
    /// Полезно гонять по большому словарю русских слов, чтобы ловить ложные срабатывания.
    #[test]
    #[ignore]
    fn vocab_scan() {
        let path = std::env::var("VOCAB").expect("VOCAB=путь к списку слов");
        let text = std::fs::read_to_string(path).unwrap();
        let hits: Vec<&str> = text.lines().filter(|w| m().count(w) > 0).collect();
        println!("{}", hits.join("\n"));
        eprintln!("совпадений: {}", hits.len());
    }
}
