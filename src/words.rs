//! Эмоции по словам. Грусть и удивление по звуку почти не отличить от обычной речи
//! (протяжные гласные), зато их выдают слова: «ого», «ничего себе», «эх», «печаль».
//! Распознанная фраза сверяется со словарём, итог решается вместе со звуком.

use std::collections::BTreeMap;

/// Эмоции, у которых есть словарь.
pub const KINDS: [&str; 3] = ["surprise", "sad", "laugh"];

/// Словарь по умолчанию: обычные русские междометия и оценки. `*` в конце — любое окончание.
/// Повторы букв не важны: «эээх» = «эх», «огооо» = «ого».
pub fn default_words() -> BTreeMap<String, Vec<String>> {
    let list = |s: &str| s.split(',').map(|w| w.trim().to_string()).collect::<Vec<_>>();
    BTreeMap::from([
        (
            "surprise".to_string(),
            list(
                "ого, вау, ух ты, ничего себе, ничоси, них* себе, ниф* себе, офиг*, обалд*, охре*, \
                 ахре*, да ладно, неужели, вот это да, шок*, ах, ох, уау, воу",
            ),
        ),
        (
            "sad".to_string(),
            list("эх, печал*, грустн*, грусть, жаль, обидн*, увы, тоск*, плак*, не повезло, грущу"),
        ),
        (
            "laugh".to_string(),
            list("смешн*, хаха*, ха ха, хе хе, хехе*, ржу, ржак*, ржач*, угар*, ору, лол, кек"),
        ),
    ])
}

/// Текст → слова: строчные, ё → е, без знаков, повторы букв схлопнуты.
pub fn tokens(text: &str) -> Vec<String> {
    let mut clean = String::with_capacity(text.len());
    for ch in text.chars().flat_map(char::to_lowercase) {
        let ch = if ch == 'ё' { 'е' } else { ch };
        clean.push(if ch.is_alphanumeric() { ch } else { ' ' });
    }
    clean
        .split_whitespace()
        .map(|w| {
            let mut out = String::with_capacity(w.len());
            let mut prev = None;
            for c in w.chars() {
                if prev != Some(c) {
                    out.push(c);
                }
                prev = Some(c);
            }
            out
        })
        .collect()
}

/// Слоги, которыми поют и мычат («ла-ла-ла», «на-на», «пам-пам»): это не слова.
const SING_SYLLABLES: [&str; 10] = ["пам", "там", "ла", "ля", "на", "ня", "ра", "па", "ту", "ду"];

fn is_filler(w: &str) -> bool {
    // «мммм», одиночные гласные — тоже не слова
    if ["м", "у", "о", "а", "э", "и", "ы"].contains(&w) {
        return true;
    }
    let mut rest = w;
    while !rest.is_empty() {
        match SING_SYLLABLES.iter().find(|s| rest.starts_with(**s)) {
            Some(s) => rest = &rest[s.len()..],
            None => return false,
        }
    }
    true
}

/// Сколько во фразе настоящих слов (без «ла-ла-ла» и одиночных гласных).
pub fn word_count(words: &[String]) -> usize {
    words.iter().filter(|w| !is_filler(w)).count()
}

#[derive(Debug, Clone)]
enum Tok {
    Exact(String),
    Prefix(String),
}

impl Tok {
    fn hit(&self, w: &str) -> bool {
        match self {
            Tok::Exact(s) => w == s,
            Tok::Prefix(s) => w.starts_with(s.as_str()),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Lexicon {
    pats: Vec<(usize, Vec<Tok>)>,
}

/// Сколько слов каждой эмоции (в порядке `KINDS`) нашлось во фразе.
pub type Counts = [usize; 3];

impl Lexicon {
    pub fn new(words: &BTreeMap<String, Vec<String>>) -> Lexicon {
        let mut pats = Vec::new();
        for (k, kind) in KINDS.iter().enumerate() {
            for phrase in words.get(*kind).into_iter().flatten() {
                let toks: Vec<Tok> = phrase
                    .split_whitespace()
                    .filter_map(|w| {
                        let prefix = w.ends_with('*');
                        let t = tokens(w.trim_end_matches('*')).into_iter().next()?;
                        Some(if prefix { Tok::Prefix(t) } else { Tok::Exact(t) })
                    })
                    .collect();
                if !toks.is_empty() {
                    pats.push((k, toks));
                }
            }
        }
        Lexicon { pats }
    }

    pub fn count(&self, words: &[String]) -> Counts {
        let mut c = [0; 3];
        for (k, toks) in &self.pats {
            if toks.len() > words.len() {
                continue;
            }
            for start in 0..=words.len() - toks.len() {
                if toks.iter().zip(&words[start..]).all(|(t, w)| t.hit(w)) {
                    c[*k] += 1;
                }
            }
        }
        c
    }
}

/// Что звучало во фразе по звуку: средние вероятности личной модели по `voice::CLASSES`
/// (neutral, shout, laugh, sad, surprise, sing) и есть ли в модели смех.
pub struct Acoustic {
    pub mean: [f32; 6],
    pub has_laugh: bool,
}

/// Итог фразы: эмоция или `None` (обычная речь / крик).
///
/// 1. Слова удивления (их не меньше, чем грусти) → удивление; слова грусти → грусть.
/// 2. Слова смеха и звук похож на смех → смех (просто «смешно» ровным голосом — нет).
/// 3. Голос без слов дольше 1,5 с и не смех → пение / мычание.
/// 4. Иначе — что слышно по звуку.
pub fn decide(c: Counts, n_words: usize, voiced_s: f32, ac: Option<&Acoustic>) -> Option<&'static str> {
    let [surprise, sad, laugh] = c;
    if surprise > 0 && surprise >= sad {
        return Some("surprise");
    }
    if sad > 0 {
        return Some("sad");
    }
    if laugh > 0 {
        match ac {
            Some(a) if a.has_laugh => {
                if a.mean[2] > 0.3 {
                    return Some("laugh");
                }
            }
            _ => return Some("laugh"),
        }
    }
    let a = ac?;
    if n_words == 0 && voiced_s >= 1.5 && a.mean[2] < 0.6 {
        return Some("sing");
    }
    let best = (0..6).max_by(|x, y| a.mean[*x].total_cmp(&a.mean[*y]))?;
    match best {
        2 => Some("laugh"),
        5 => Some("sing"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex() -> Lexicon {
        Lexicon::new(&default_words())
    }

    fn count(s: &str) -> Counts {
        lex().count(&tokens(s))
    }

    #[test]
    fn tokens_normalize() {
        assert_eq!(tokens("Эээх, Ёлки-палки!"), vec!["эх", "елки", "палки"]);
    }

    #[test]
    fn finds_emotion_words() {
        assert_eq!(count("ничего себе ого"), [2, 0, 0]);
        assert_eq!(count("нихуя себе ох"), [2, 0, 0]);
        assert_eq!(count("эх да эх да да да"), [0, 2, 0]);
        assert_eq!(count("печаль конечно печаль"), [0, 2, 0]);
        assert_eq!(count("смешно конечно да"), [0, 0, 1]);
        assert_eq!(count("хахахаха"), [0, 0, 1]);
        assert_eq!(count("прикольно сегодня поиграли"), [0, 0, 0]);
        assert_eq!(count("ну сколько можно"), [0, 0, 0]);
    }

    #[test]
    fn singing_syllables_are_not_words() {
        assert_eq!(word_count(&tokens("ла ла ла лалала")), 0);
        assert_eq!(word_count(&tokens("пам-пам мммм")), 0);
        assert_eq!(word_count(&tokens("а смешно что")), 2);
        assert_eq!(word_count(&tokens("лампа")), 1);
    }

    #[test]
    fn decision_rules() {
        let ac = |l: f32, s: f32| Acoustic {
            mean: [1.0 - l - s, 0.0, l, 0.0, 0.0, s],
            has_laugh: true,
        };
        assert_eq!(decide([1, 0, 0], 3, 2.0, Some(&ac(0.9, 0.0))), Some("surprise"));
        assert_eq!(decide([0, 2, 0], 3, 2.0, None), Some("sad"));
        assert_eq!(decide([0, 0, 1], 3, 2.0, Some(&ac(0.1, 0.0))), None);
        assert_eq!(decide([0, 0, 1], 3, 2.0, Some(&ac(0.5, 0.0))), Some("laugh"));
        assert_eq!(decide([0, 0, 0], 0, 3.0, Some(&ac(0.2, 0.1))), Some("sing"));
        assert_eq!(decide([0, 0, 0], 0, 3.0, Some(&ac(0.8, 0.1))), Some("laugh"));
        assert_eq!(decide([0, 0, 0], 0, 1.0, Some(&ac(0.1, 0.1))), None);
        assert_eq!(decide([0, 0, 0], 5, 3.0, None), None);
    }
}
