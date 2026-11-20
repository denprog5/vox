//! Личная модель голоса.
//!
//! Пользователь записывает по несколько коротких примеров: обычная речь, крик, смех, пение.
//! Записи хранятся как WAV (`%APPDATA%\Vox\voice\<класс>\*.wav`), поэтому модель
//! всегда можно пересчитать заново. Грусть и удивление по звуку от речи почти не отличить —
//! их узнаём по словам (`words.rs`), записи этих классов в звуковую модель не идут.
//!
//! Каждая запись режется на 2-секундные окна с шагом 0,25 с (только где звучит голос).
//! Признаки окна — логиты всех 527 классов CED и 16 голосовых признаков (`prosody.rs`).
//! По ним учится многоклассовая логистическая регрессия: классы соревнуются между собой,
//! и смех не может одновременно быть криком.
//! Точность честно меряется перекрёстной проверкой: запись, на которой проверяем, в обучение
//! не попадает.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::app_dir;
use crate::prosody;
use crate::tagger::{self, Tagger};

pub const CLASSES: [&str; 6] = ["neutral", "shout", "laugh", "sad", "surprise", "sing"];
pub const NEUTRAL: usize = 0;
pub const SHOUT: usize = 1;
/// Классы, которые различаются по звуку: обычная речь, крик, смех, пение.
pub const ACOUSTIC: [usize; 4] = [0, 1, 2, 5];
/// Сколько записей класса нужно, чтобы он участвовал в модели.
pub const MIN_TAKES: usize = 2;
/// Больше не храним: самые старые записи уходят.
pub const MAX_TAKES: usize = 12;

pub const WIN: usize = 2 * tagger::RATE as usize;
const HOP: usize = tagger::RATE as usize / 4;
const FRAME: usize = 320;
pub const FOLDS: usize = 4;

pub fn class_index(name: &str) -> Option<usize> {
    CLASSES.iter().position(|c| *c == name)
}

pub fn dir() -> PathBuf {
    app_dir().join("voice")
}

#[derive(Clone)]
pub struct Take {
    pub class: usize,
    pub path: PathBuf,
    pub feats: Vec<Vec<f32>>,
    /// Окна той же записи, проигранной на 8% быстрее и медленнее: голос ведь не всегда
    /// звучит одинаково, а записей мало.
    pub aug: Vec<Vec<f32>>,
}

/// Данные одной записи для обучения: класс, окна, дополнительные окна.
pub type TakeData = (usize, Vec<Vec<f32>>, Vec<Vec<f32>>);

pub fn take_data(takes: &[Take]) -> Vec<TakeData> {
    takes
        .iter()
        .map(|t| (t.class, t.feats.clone(), t.aug.clone()))
        .collect()
}

/// Все записи по классам, старые первыми.
fn list_files() -> Vec<(usize, PathBuf)> {
    let mut out = Vec::new();
    for (ci, c) in CLASSES.iter().enumerate() {
        let Ok(rd) = std::fs::read_dir(dir().join(c)) else {
            continue;
        };
        let mut files: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "wav"))
            .collect();
        files.sort();
        out.extend(files.into_iter().map(|p| (ci, p)));
    }
    out
}

pub fn load_takes(t: &Tagger) -> Vec<Take> {
    list_files()
        .into_iter()
        .filter_map(|(class, path)| {
            let (feats, aug) = match read_cache(&path, cache_ver(t), t.names.len() + prosody::LEN) {
                Some(c) => c,
                None => {
                    let (pcm, rate) = read_wav(&path).ok()?;
                    if rate != tagger::RATE as u32 {
                        return None;
                    }
                    let (feats, aug, _) = take_features(t, &pcm);
                    write_cache(&path, cache_ver(t), &feats, &aug);
                    (feats, aug)
                }
            };
            (!feats.is_empty()).then_some(Take {
                class,
                path,
                feats,
                aug,
            })
        })
        .collect()
}

/// Удалить запись вместе с кэшем признаков.
pub fn remove_take(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("feat"));
}

// Кэш признаков рядом с WAV: `u32 версия, u32 окон, u32 доп. окон, u32 d`, затем f32.
const CACHE_VER: u32 = 2;

/// Версия кэша вместе с отпечатком модели звуков.
pub fn cache_ver(t: &Tagger) -> u32 {
    CACHE_VER | (t.tag << 8)
}

pub fn write_cache(wav: &Path, ver: u32, feats: &[Vec<f32>], aug: &[Vec<f32>]) {
    let d = feats.first().map_or(0, Vec::len);
    let mut b = Vec::with_capacity(16 + (feats.len() + aug.len()) * d * 4);
    for v in [ver, feats.len() as u32, aug.len() as u32, d as u32] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    for row in feats.iter().chain(aug) {
        for x in row {
            b.extend_from_slice(&x.to_le_bytes());
        }
    }
    let _ = std::fs::write(wav.with_extension("feat"), b);
}

fn read_cache(wav: &Path, ver: u32, d_expect: usize) -> Option<(Vec<Vec<f32>>, Vec<Vec<f32>>)> {
    let b = std::fs::read(wav.with_extension("feat")).ok()?;
    let u = |i: usize| -> Option<usize> {
        Some(u32::from_le_bytes(b.get(i * 4..i * 4 + 4)?.try_into().ok()?) as usize)
    };
    if u(0)? != ver as usize || u(3)? != d_expect {
        return None;
    }
    let (n, m, d) = (u(1)?, u(2)?, u(3)?);
    if b.len() != 16 + (n + m) * d * 4 {
        return None;
    }
    let mut rows = b[16..]
        .chunks_exact(d * 4)
        .map(|r| {
            r.chunks_exact(4)
                .map(|x| f32::from_le_bytes([x[0], x[1], x[2], x[3]]))
                .collect::<Vec<f32>>()
        })
        .collect::<Vec<_>>();
    let aug = rows.split_off(n);
    Some((rows, aug))
}

/// Проиграть запись быстрее (`k` > 1) или медленнее: меняются и темп, и высота голоса.
fn speed(pcm: &[f32], k: f32) -> Vec<f32> {
    let n = (pcm.len() as f32 / k) as usize;
    (0..n)
        .map(|i| {
            let x = i as f32 * k;
            let j = x as usize;
            let f = x - j as f32;
            let a = pcm[j.min(pcm.len() - 1)];
            let b = pcm[(j + 1).min(pcm.len() - 1)];
            a + (b - a) * f
        })
        .collect()
}

pub fn save_take(class: usize, pcm: &[f32]) -> Result<PathBuf, String> {
    let d = dir().join(CLASSES[class]);
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let p = d.join(format!("{ms}.wav"));
    write_wav(&p, pcm, tagger::RATE as u32).map_err(|e| e.to_string())?;
    Ok(p)
}

fn frame_db(f: &[f32]) -> f32 {
    let rms = (f.iter().map(|x| x * x).sum::<f32>() / f.len().max(1) as f32).sqrt();
    if rms > 1e-6 {
        20.0 * rms.log10()
    } else {
        -120.0
    }
}

/// 2-секундные окна, где голос звучит хотя бы четверть времени. Если таких нет
/// (короткое «ах!»), берётся окно вокруг самого громкого места.
pub fn voiced_windows(pcm: &[f32]) -> Vec<&[f32]> {
    if pcm.len() <= WIN {
        return vec![pcm];
    }
    let db: Vec<f32> = pcm.chunks(FRAME).map(frame_db).collect();
    let peak = db.iter().copied().fold(-120.0f32, f32::max);
    let thr = (peak - 25.0).max(-65.0);
    let per_win = WIN / FRAME;
    let mut out = Vec::new();
    let mut best = (0usize, -1.0f32);
    let mut i = 0;
    while i + WIN <= pcm.len() {
        let f0 = i / FRAME;
        let fr = &db[f0..(f0 + per_win).min(db.len())];
        let frac = fr.iter().filter(|d| **d > thr).count() as f32 / per_win as f32;
        if frac >= 0.25 {
            out.push(&pcm[i..i + WIN]);
        }
        if frac > best.1 {
            best = (i, frac);
        }
        i += HOP;
    }
    if out.is_empty() && best.1 > 0.0 {
        out.push(&pcm[best.0..best.0 + WIN]);
    }
    out
}

/// Признаки окон записи, окон «ускоренной/замедленной» копии и средние вероятности
/// классов (чтобы показать, что услышано).
pub fn take_features(t: &Tagger, pcm: &[f32]) -> (Vec<Vec<f32>>, Vec<Vec<f32>>, Vec<f32>) {
    let wins = voiced_windows(pcm);
    let mut mean = vec![0.0f32; t.names.len()];
    let feats = wins
        .iter()
        .map(|w| {
            let p = t.probs(w);
            for (m, x) in mean.iter_mut().zip(&p) {
                *m += x / wins.len() as f32;
            }
            features(&p, w)
        })
        .collect();
    let mut aug = Vec::new();
    if !pcm.is_empty() {
        for k in [0.92f32, 1.08] {
            let v = speed(pcm, k);
            aug.extend(voiced_windows(&v).iter().map(|w| features(&t.probs(w), w)));
        }
    }
    (feats, aug, mean)
}

/// Признаки окна: логиты CED (в отличие от вероятностей, различия между «почти 0» и
/// «чуть больше» не теряются) и голосовые признаки того же окна.
pub fn features(probs: &[f32], seg: &[f32]) -> Vec<f32> {
    let mut f: Vec<f32> = probs
        .iter()
        .map(|&p| {
            let p = p.clamp(1e-4, 1.0 - 1e-4);
            (p / (1.0 - p)).ln()
        })
        .collect();
    f.extend_from_slice(&prosody::features(seg));
    f
}

// ---------------------------------------------------------------------------------------------
// Классификатор

#[derive(Clone)]
pub struct Model {
    /// Индексы в `CLASSES`, которые модель различает.
    pub classes: Vec<usize>,
    d: usize,
    mean: Vec<f32>,
    scale: Vec<f32>,
    w: Vec<f32>,
    b: Vec<f32>,
    /// Обучающие окна (уже нормированные) — чтобы понять, похож ли звук хоть на что-то знакомое.
    train_x: Vec<Vec<f32>>,
    /// Типичное расстояние до ближайшего окна из другой записи (90-й процентиль).
    nn_dist: f32,
}

const L2: f32 = 0.02;
const ITERS: usize = 250;

impl Model {
    pub fn has(&self, class: usize) -> bool {
        self.classes.contains(&class)
    }

    /// Многоклассовая логистическая регрессия: градиентный спуск с моментом Нестерова,
    /// веса классов выровнены (класс с большим числом окон не перетягивает).
    fn train(
        x: &[&[f32]],
        y: &[usize],
        groups: &[usize],
        real: &[bool],
        classes: Vec<usize>,
    ) -> Model {
        let k = classes.len();
        let d = x[0].len();
        let n = x.len();
        let mut mean = vec![0.0f32; d];
        for xi in x {
            for (m, v) in mean.iter_mut().zip(xi.iter()) {
                *m += v / n as f32;
            }
        }
        let mut var = vec![0.0f32; d];
        for xi in x {
            for j in 0..d {
                let t = xi[j] - mean[j];
                var[j] += t * t / n as f32;
            }
        }
        let scale: Vec<f32> = var.iter().map(|v| 1.0 / v.sqrt().max(0.3)).collect();
        let xs: Vec<Vec<f32>> = x
            .iter()
            .map(|xi| (0..d).map(|j| (xi[j] - mean[j]) * scale[j]).collect())
            .collect();
        let mut count = vec![0usize; k];
        for &c in y {
            count[c] += 1;
        }
        let cw: Vec<f32> = count
            .iter()
            .map(|&c| n as f32 / (k as f32 * c.max(1) as f32))
            .collect();
        let wsum: f32 = y.iter().map(|&c| cw[c]).sum();
        let l = xs.iter().map(|v| v.iter().map(|a| a * a).sum::<f32>()).sum::<f32>() / n as f32;
        let lr = 1.0 / (0.5 * l + L2);

        let mut w = vec![0.0f32; k * d];
        let mut b = vec![0.0f32; k];
        let mut vw = vec![0.0f32; k * d];
        let mut vb = vec![0.0f32; k];
        let mut gw = vec![0.0f32; k * d];
        let mut gb = vec![0.0f32; k];
        let mut look = vec![0.0f32; k * d];
        let mut lookb = vec![0.0f32; k];
        let mut z = vec![0.0f32; k];
        const MOM: f32 = 0.9;
        for _ in 0..ITERS {
            for i in 0..k * d {
                look[i] = w[i] + MOM * vw[i];
            }
            for c in 0..k {
                lookb[c] = b[c] + MOM * vb[c];
            }
            gw.iter_mut().for_each(|g| *g = 0.0);
            gb.iter_mut().for_each(|g| *g = 0.0);
            for (xi, &yi) in xs.iter().zip(y) {
                for c in 0..k {
                    z[c] = lookb[c] + dot(&look[c * d..(c + 1) * d], xi);
                }
                softmax(&mut z);
                let wi = cw[yi] / wsum;
                for c in 0..k {
                    let g = (z[c] - if c == yi { 1.0 } else { 0.0 }) * wi;
                    if g.abs() < 1e-7 {
                        continue;
                    }
                    gb[c] += g;
                    for (gwj, xj) in gw[c * d..(c + 1) * d].iter_mut().zip(xi) {
                        *gwj += g * xj;
                    }
                }
            }
            for i in 0..k * d {
                let g = gw[i] + L2 * look[i];
                vw[i] = MOM * vw[i] - lr * g;
                w[i] += vw[i];
            }
            for c in 0..k {
                vb[c] = MOM * vb[c] - lr * gb[c];
                b[c] += vb[c];
            }
        }
        // Расстояние каждого настоящего окна до ближайшего настоящего окна из другой записи.
        let refs: Vec<usize> = (0..n).filter(|&i| real[i]).collect();
        let mut nn: Vec<f32> = refs
            .iter()
            .map(|&i| {
                refs.iter()
                    .filter(|&&j| groups[j] != groups[i])
                    .map(|&j| dist(&xs[i], &xs[j]))
                    .fold(f32::MAX, f32::min)
            })
            .filter(|v| *v < f32::MAX)
            .collect();
        nn.sort_by(f32::total_cmp);
        let nn_dist = nn
            .get((nn.len() as f32 * 0.9) as usize)
            .or(nn.last())
            .copied()
            .unwrap_or(1.0);
        let train_x: Vec<Vec<f32>> = refs.iter().map(|&i| xs[i].clone()).collect();
        Model {
            classes,
            d,
            mean,
            scale,
            w,
            b,
            train_x,
            nn_dist,
        }
    }

    fn standardize(&self, x: &[f32]) -> Vec<f32> {
        (0..self.d)
            .map(|j| (x[j] - self.mean[j]) * self.scale[j])
            .collect()
    }

    /// 1 — звук похож на записи пользователя, 0 — совсем незнакомый (шум игры, музыка,
    /// посторонний звук). Незнакомое не должно превращаться в эмоцию.
    pub fn familiarity(&self, x: &[f32]) -> f32 {
        let xs = self.standardize(x);
        let d = self
            .train_x
            .iter()
            .map(|t| dist(&xs, t))
            .fold(f32::MAX, f32::min);
        let (lo, hi) = (self.nn_dist * 1.3, self.nn_dist * 2.2);
        ((hi - d) / (hi - lo)).clamp(0.0, 1.0)
    }

    /// Вероятности классов `self.classes` (в том же порядке).
    pub fn predict(&self, x: &[f32]) -> Vec<f32> {
        let xs = self.standardize(x);
        let mut z: Vec<f32> = (0..self.classes.len())
            .map(|c| self.b[c] + dot(&self.w[c * self.d..(c + 1) * self.d], &xs))
            .collect();
        softmax(&mut z);
        z
    }

    /// Средняя вероятность по нескольким окнам, разложенная по `CLASSES`.
    pub fn predict_all(&self, feats: &[Vec<f32>]) -> [f32; 6] {
        let mut out = [0.0f32; 6];
        for f in feats {
            for (i, p) in self.predict(f).into_iter().enumerate() {
                out[self.classes[i]] += p / feats.len().max(1) as f32;
            }
        }
        out
    }

    /// Как `predict_all` для одного окна, но незнакомый звук уводится в «обычную речь».
    /// Возвращает вероятности и знакомость.
    pub fn predict_live(&self, x: &[f32]) -> ([f32; 6], f32) {
        let mut p = self.predict_all(std::slice::from_ref(&x.to_vec()));
        let fam = self.familiarity(x);
        let mut moved = 0.0;
        for (c, v) in p.iter_mut().enumerate() {
            if c != NEUTRAL {
                moved += *v * (1.0 - fam);
                *v *= fam;
            }
        }
        p[NEUTRAL] += moved;
        (p, fam)
    }
}

/// Средний квадрат разности по признакам.
fn dist(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum::<f32>() / a.len().max(1) as f32
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    prosody::dot(a, b)
}

fn softmax(z: &mut [f32]) {
    let m = z.iter().copied().fold(f32::MIN, f32::max);
    let mut s = 0.0;
    for v in z.iter_mut() {
        *v = (*v - m).exp();
        s += *v;
    }
    for v in z.iter_mut() {
        *v /= s;
    }
}

// ---------------------------------------------------------------------------------------------
// Обучение + честная проверка точности

#[derive(Clone, Default)]
pub struct Report {
    pub takes: [usize; 6],
    /// Доля верно угаданных окон на записях, которых модель не видела.
    pub acc: [Option<f32>; 6],
    /// С чем класс путается чаще всего (если заметно).
    pub confused: [Option<usize>; 6],
    pub overall: Option<f32>,
    /// confusion[истинный][угаданный], доли по строке.
    pub confusion: [[f32; 6]; 6],
}

/// Классы звуковой модели: звуковые классы, у которых достаточно записей.
pub fn model_classes(takes: &[TakeData]) -> Vec<usize> {
    ACOUSTIC
        .into_iter()
        .filter(|&c| takes.iter().filter(|t| t.0 == c).count() >= MIN_TAKES)
        .collect()
}

/// Номер «складки» записи для проверки: по порядку внутри класса, чтобы в каждом
/// обучении оставались записи всех классов.
pub fn folds(takes: &[TakeData]) -> Vec<usize> {
    let mut seen = [0usize; 6];
    takes
        .iter()
        .map(|(c, ..)| {
            seen[*c] += 1;
            (seen[*c] - 1) % FOLDS
        })
        .collect()
}

/// Обучить модель на записях классов `classes`, для которых `use_take(номер)`.
pub fn fit(takes: &[TakeData], classes: &[usize], use_take: &dyn Fn(usize) -> bool) -> Model {
    let mut x: Vec<&[f32]> = Vec::new();
    let mut y = Vec::new();
    let mut g = Vec::new();
    let mut real = Vec::new();
    for (ti, (c, feats, aug)) in takes.iter().enumerate() {
        let Some(lc) = classes.iter().position(|x| x == c) else {
            continue;
        };
        if !use_take(ti) {
            continue;
        }
        for (f, is_real) in feats.iter().map(|f| (f, true)).chain(aug.iter().map(|f| (f, false))) {
            x.push(f);
            y.push(lc);
            g.push(ti);
            real.push(is_real);
        }
    }
    Model::train(&x, &y, &g, &real, classes.to_vec())
}

/// `(класс, окна записи)` → модель и отчёт о точности.
pub fn build(takes: &[TakeData]) -> (Option<Model>, Report) {
    let mut rep = Report::default();
    for (c, ..) in takes {
        rep.takes[*c] += 1;
    }
    let classes = model_classes(takes);
    if !classes.contains(&NEUTRAL) || classes.len() < 2 {
        return (None, rep);
    }
    let local = |c: usize| classes.iter().position(|x| *x == c);
    let fold = folds(takes);
    let mut conf = [[0.0f32; 6]; 6];
    for f in 0..FOLDS {
        if !takes.iter().enumerate().any(|(ti, (c, ..))| fold[ti] == f && local(*c).is_some()) {
            continue;
        }
        let m = fit(takes, &classes, &|ti| fold[ti] != f);
        for (ti, (c, feats, _)) in takes.iter().enumerate() {
            if fold[ti] != f || local(*c).is_none() {
                continue;
            }
            for x in feats {
                let p = m.predict(x);
                let best = argmax(&p);
                conf[*c][classes[best]] += 1.0 / feats.len() as f32;
            }
        }
    }
    let mut sum = 0.0;
    for &c in &classes {
        let row: f32 = conf[c].iter().sum();
        if row <= 0.0 {
            continue;
        }
        for v in conf[c].iter_mut() {
            *v /= row;
        }
        rep.acc[c] = Some(conf[c][c]);
        sum += conf[c][c];
        let (oc, ov) = (0..6)
            .filter(|&o| o != c)
            .map(|o| (o, conf[c][o]))
            .fold((0, 0.0), |a, b| if b.1 > a.1 { b } else { a });
        if ov >= 0.1 {
            rep.confused[c] = Some(oc);
        }
    }
    rep.overall = Some(sum / classes.len() as f32);
    rep.confusion = conf;
    (Some(fit(takes, &classes, &|_| true)), rep)
}

fn argmax(v: &[f32]) -> usize {
    v.iter()
        .enumerate()
        .fold((0, f32::MIN), |a, (i, &x)| if x > a.1 { (i, x) } else { a })
        .0
}

// ---------------------------------------------------------------------------------------------
// WAV

pub fn write_wav(path: &Path, pcm: &[f32], rate: u32) -> std::io::Result<()> {
    let data_len = (pcm.len() * 2) as u32;
    let mut b = Vec::with_capacity(44 + data_len as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        b.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, b)?;
    std::fs::rename(tmp, path)
}

/// 16-битный PCM WAV → моно f32 и частота.
pub fn read_wav(path: &Path) -> Result<(Vec<f32>, u32), String> {
    let b = std::fs::read(path).map_err(|e| e.to_string())?;
    if b.len() < 44 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err("не WAV".into());
    }
    let (mut ch, mut rate, mut bits, mut pos) = (1u16, 16000u32, 16u16, 12usize);
    while pos + 8 <= b.len() {
        let id = &b[pos..pos + 4];
        let size = u32::from_le_bytes(b[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = &b[pos + 8..(pos + 8 + size).min(b.len())];
        if id == b"fmt " && body.len() >= 16 {
            ch = u16::from_le_bytes([body[2], body[3]]).max(1);
            rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            bits = u16::from_le_bytes([body[14], body[15]]);
        } else if id == b"data" {
            if bits != 16 {
                return Err("нужен 16-битный WAV".into());
            }
            let samples: Vec<f32> = body
                .chunks_exact(2 * ch as usize)
                .map(|f| {
                    f.chunks_exact(2)
                        .map(|s| i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0)
                        .sum::<f32>()
                        / ch as f32
                })
                .collect();
            return Ok((samples, rate));
        }
        pos += 8 + size + (size & 1);
    }
    Err("в WAV нет данных".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_roundtrip() {
        let p = std::env::temp_dir().join("vox-test.wav");
        let pcm: Vec<f32> = (0..1600).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
        write_wav(&p, &pcm, 16000).unwrap();
        let (back, rate) = read_wav(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        assert_eq!(rate, 16000);
        assert_eq!(back.len(), pcm.len());
        assert!(back.iter().zip(&pcm).all(|(a, b)| (a - b).abs() < 1e-3));
    }

    #[test]
    fn voiced_windows_skip_silence() {
        let mut pcm = vec![0.0f32; 16000 * 8];
        for (i, s) in pcm[16000..32000].iter_mut().enumerate() {
            *s = (i as f32 * 0.07).sin() * 0.3;
        }
        let w = voiced_windows(&pcm);
        assert!(!w.is_empty());
        // из 25 окон по 2 с — только те, где звук (1-2 с) занимает хотя бы 0,5 с
        assert!(w.len() <= 7, "{}", w.len());
    }

    /// Классификатор различает три синтетических «звука» и честно меряет точность.
    #[test]
    fn classifier_separates_clusters() {
        let mut rng = 1u32;
        let mut rnd = move || {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            (rng as f32 / u32::MAX as f32) - 0.5
        };
        let mut takes = Vec::new();
        for (c, center) in [(NEUTRAL, 0usize), (SHOUT, 1), (2, 2)] {
            for _ in 0..4 {
                let feats = (0..10)
                    .map(|_| {
                        (0..20)
                            .map(|j| if j == center { 3.0 } else { 0.0 } + rnd() * 2.0)
                            .collect()
                    })
                    .collect();
                takes.push((c, feats, Vec::new()));
            }
        }
        let (m, rep) = build(&takes);
        let m = m.unwrap();
        assert_eq!(m.classes, vec![0, 1, 2]);
        assert!(rep.overall.unwrap() > 0.9, "{:?}", rep.acc);
        assert_eq!(rep.takes[0], 4);
    }

    #[test]
    fn feature_cache_roundtrip() {
        let p = std::env::temp_dir().join("vox-cache-test.wav");
        let feats = vec![vec![1.0, 2.0, 3.0], vec![4.0, 5.0, 6.0]];
        let aug = vec![vec![7.0, 8.0, 9.0]];
        write_cache(&p, 7, &feats, &aug);
        let (f, a) = read_cache(&p, 7, 3).unwrap();
        assert!(read_cache(&p, 7, 4).is_none());
        assert!(read_cache(&p, 8, 3).is_none());
        remove_take(&p);
        assert_eq!((f, a), (feats, aug));
    }

    #[test]
    fn speed_changes_length() {
        let pcm = vec![0.5f32; 1000];
        assert_eq!(speed(&pcm, 1.25).len(), 800);
        assert_eq!(speed(&pcm, 0.8).len(), 1250);
    }

    #[test]
    fn no_model_without_neutral() {
        let takes = vec![
            (SHOUT, vec![vec![1.0; 4]], Vec::new()),
            (SHOUT, vec![vec![1.0; 4]], Vec::new()),
        ];
        assert!(build(&takes).0.is_none());
    }
}
