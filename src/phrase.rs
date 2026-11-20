//! Сборка фраз из потока: голос (с 0,3 с до него) копится до паузы 0,5 с. Слишком длинная
//! фраза режется в самом тихом месте последних 1,5 с, чтобы не разрубить слово. Готовая
//! фраза уходит в распознавание речи. Работает на 16 кГц, время считается в отсчётах —
//! одинаково и вживую, и при проверке на файле.

use std::collections::VecDeque;

const RATE: usize = 16_000;
const PREROLL: usize = RATE * 3 / 10;
const PAUSE: usize = RATE / 2;
/// Тишину в конце распознавать незачем: оставляем 0,2 с.
const KEEP_TAIL: usize = RATE / 5;
const MAX_LEN: usize = RATE * 6;
const CUT_SEARCH: usize = RATE * 3 / 2;
const CUT_WINDOW: usize = RATE / 100;
/// Короче — не распознаём (щелчок, стук). «Бля» — около 0,2 с.
const MIN_VOICED: usize = RATE * 15 / 100;

pub struct Done {
    pub id: u64,
    pub pcm: Vec<f32>,
    /// Сколько секунд в фразе звучал голос.
    pub voiced_s: f32,
}

#[derive(Default)]
pub struct PhraseBuilder {
    preroll: VecDeque<f32>,
    pcm: Vec<f32>,
    open: bool,
    id: u64,
    voiced: usize,
    silence: usize,
}

impl PhraseBuilder {
    /// Номер текущей (открытой) фразы.
    pub fn current(&self) -> Option<u64> {
        self.open.then_some(self.id)
    }

    /// Кадр (16 кГц) и есть ли в нём голос. Возвращает законченную фразу.
    pub fn push(&mut self, frame: &[f32], active: bool) -> Option<Done> {
        if !self.open {
            if !active {
                self.preroll.extend(frame);
                let extra = self.preroll.len().saturating_sub(PREROLL);
                self.preroll.drain(..extra);
                return None;
            }
            self.open = true;
            self.id += 1;
            self.pcm.clear();
            self.pcm.extend(self.preroll.drain(..));
            self.voiced = 0;
            self.silence = 0;
        }
        self.pcm.extend_from_slice(frame);
        if active {
            self.voiced += frame.len();
            self.silence = 0;
        } else {
            self.silence += frame.len();
        }
        if self.silence >= PAUSE {
            let keep = self.pcm.len() - self.silence + KEEP_TAIL.min(self.silence);
            self.pcm.truncate(keep);
            return self.close();
        }
        if self.pcm.len() >= MAX_LEN {
            let cut = quietest_point(&self.pcm);
            let rest = self.pcm.split_off(cut);
            // голос остатка примерно равен его длине: режем посреди речи
            let rest_voiced = rest.len().min(self.voiced);
            self.voiced -= rest_voiced;
            let done = self.close();
            self.open = true;
            self.id += 1;
            self.pcm = rest;
            self.voiced = rest_voiced;
            self.silence = 0;
            return done;
        }
        None
    }

    /// Закончить фразу сразу (остановка микрофона, конец файла).
    pub fn flush(&mut self) -> Option<Done> {
        if self.open { self.close() } else { None }
    }

    fn close(&mut self) -> Option<Done> {
        self.open = false;
        let pcm = std::mem::take(&mut self.pcm);
        (self.voiced >= MIN_VOICED).then(|| Done {
            id: self.id,
            pcm,
            voiced_s: self.voiced as f32 / RATE as f32,
        })
    }
}

/// Начало самого тихого 10-мс окна в последних 1,5 с.
fn quietest_point(pcm: &[f32]) -> usize {
    let start = pcm.len().saturating_sub(CUT_SEARCH);
    let energy = |a: usize| pcm[a..a + CUT_WINDOW].iter().map(|x| x * x).sum::<f32>();
    (start..pcm.len().saturating_sub(CUT_WINDOW))
        .step_by(CUT_WINDOW)
        .min_by(|&a, &b| energy(a).total_cmp(&energy(b)))
        .unwrap_or(pcm.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(b: &mut PhraseBuilder, secs: f32, active: bool, out: &mut Vec<Done>) {
        let amp = if active { 0.3 } else { 0.0 };
        for _ in 0..(secs * 100.0) as usize {
            if let Some(d) = b.push(&[amp; 160], active) {
                out.push(d);
            }
        }
    }

    #[test]
    fn phrase_with_preroll_and_trimmed_tail() {
        let (mut b, mut out) = (PhraseBuilder::default(), Vec::new());
        feed(&mut b, 1.0, false, &mut out);
        feed(&mut b, 1.0, true, &mut out);
        assert!(b.current().is_some());
        feed(&mut b, 1.0, false, &mut out);
        assert_eq!(out.len(), 1);
        let d = &out[0];
        // 0,3 с до + 1 с голоса + 0,2 с хвоста
        assert_eq!(d.pcm.len(), PREROLL + RATE + KEEP_TAIL);
        assert!((d.voiced_s - 1.0).abs() < 0.02);
    }

    #[test]
    fn long_speech_is_cut_and_continues() {
        let (mut b, mut out) = (PhraseBuilder::default(), Vec::new());
        feed(&mut b, 4.8, true, &mut out);
        feed(&mut b, 0.2, false, &mut out); // короткая пауза — место разреза
        feed(&mut b, 3.0, true, &mut out);
        assert_eq!(out.len(), 1, "фраза длиннее 6 с режется");
        let first = out[0].pcm.len();
        assert!((RATE * 48 / 10..=RATE * 5).contains(&first), "разрез в паузе: {first}");
        feed(&mut b, 1.0, false, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id + 1, out[1].id);
    }

    #[test]
    fn click_is_ignored() {
        let (mut b, mut out) = (PhraseBuilder::default(), Vec::new());
        feed(&mut b, 0.05, true, &mut out);
        feed(&mut b, 1.0, false, &mut out);
        assert!(out.is_empty());
    }
}
