//! Поток анализа: микрофон → громкость (рот открыт или нет) → звук (крик, смех, пение)
//! и слова (удивление, грусть, смех) → итоговое состояние кошки → страницы по WebSocket.
//!
//! Звук: оценка последних 2 секунд раз в 0,5 с (~11 мс одного ядра). Слова: голос
//! собирается во фразы (до паузы 0,5 с), фраза уходит в распознавание речи в отдельном
//! потоке; по словам и звуку фразы решается эмоция, по словам же считается мат для OBS.
//! В тишине не работает ничего.
//! Личная модель голоса учится в отдельном коротком потоке и не мешает анализу.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use serde_json::json;
use sherpa_onnx::LinearResampler;

use crate::asr::{self, Asr, AsrMsg};
use crate::config::{Config, EMOTIONS, SOUND_MODEL, WORDS_MODEL};
use crate::counter::Counter;
use crate::hub::SharedHub;
use crate::matcher::Matcher;
use crate::phrase::PhraseBuilder;
use crate::tagger::{self, Tagger};
use crate::voice::{self, Model, Report, Take};
use crate::win;
use crate::words::{self, Acoustic, Lexicon};

const FRAME_MS: u32 = 10;
/// Окно для распознавания звуков и шаг между оценками.
const WINDOW: usize = voice::WIN;
const TAG_EVERY_FRAMES: u32 = 50;
/// Слова фразы, распознанные позже этого, эмоцию уже не включают (мат всё равно считается).
const WORDS_LATE: Duration = Duration::from_secs(3);
/// Сколько фраз может ждать распознавания (≈ минута речи).
const PENDING_MAX: usize = 32;
/// Эмоция по словам включается только для фраз не короче этого.
const EMO_MIN_VOICED: f32 = 0.3;
/// Сколько секунд записывать пример для обучения. Обычной речи — дольше: чем она
/// разнообразнее, тем реже обычные слова принимаются за эмоцию.
fn train_seconds(class: &str) -> f32 {
    if class == "neutral" { 8.0 } else { 4.0 }
}
/// Сколько держать «кричит» после последнего окна, где нейросеть услышала крик.
const NET_SHOUT_HOLD: Duration = Duration::from_millis(700);

/// Порог уверенности личной модели: чувствительность 1 → 0,75 … 10 → 0,39.
fn post_thr(sens: u8) -> f32 {
    0.75 - (sens.clamp(1, 10) as f32 - 1.0) * 0.04
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum ModelState {
    #[default]
    Loading,
    Ready,
    Error(String),
}

pub type SharedModel = Arc<Mutex<ModelState>>;

pub enum Ctrl {
    Source {
        consumer: rtrb::Consumer<f32>,
        rate: u32,
    },
    Stop,
    Config(Box<Config>),
    /// Эмоция вручную (кнопка, горячая клавиша): имя, "eat" или "none".
    Manual(String),
    /// Громкость источника донатов в OBS, дБ.
    DonationDb(f32),
    Train(String),
    /// Удалить записи класса; `"<класс>#last"` — только последнюю.
    Untrain(String),
    /// Обнулить счётчик мата.
    SwearReset,
    /// В OBS началась трансляция.
    StreamStarted,
}

pub struct Engine {
    thread: Thread,
    tx: Sender<Ctrl>,
}

impl Engine {
    pub fn spawn(model_dir: PathBuf, cfg: Config, hub: SharedHub, model: SharedModel) -> Engine {
        let (tx, rx) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("engine".into())
            .spawn(move || {
                win::lower_current_thread();
                let mut w = Worker::new(cfg, hub);
                // Модель слов грузится параллельно в своём потоке (~1 с).
                w.asr = Some(Asr::spawn(model_dir.join(WORDS_MODEL), thread::current()));
                // Громкость работает сразу, звуки — когда загрузится модель (~0,2 с).
                let loaded = Tagger::load(&model_dir.join(SOUND_MODEL));
                *model.lock().unwrap() = match &loaded {
                    Ok(_) => ModelState::Ready,
                    Err(e) => ModelState::Error(e.clone()),
                };
                win::notify_main(win::EV_REFRESH);
                w.tagger = loaded.ok();
                w.load_voice();
                w.run(rx);
            })
            .expect("spawn engine");
        Engine {
            thread: handle.thread().clone(),
            tx,
        }
    }

    pub fn thread(&self) -> Thread {
        self.thread.clone()
    }

    pub fn send(&self, c: Ctrl) {
        let _ = self.tx.send(c);
        self.thread.unpark();
    }

    /// Отправитель для других потоков (OBS).
    pub fn sender(&self) -> crate::obs::EngineTx {
        let tx = self.tx.clone();
        let th = self.thread.clone();
        Arc::new(move |c| {
            let _ = tx.send(c);
            th.unpark();
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Voice {
    Quiet,
    Talk,
    Shout,
}

impl Voice {
    fn name(self) -> &'static str {
        match self {
            Voice::Quiet => "quiet",
            Voice::Talk => "talk",
            Voice::Shout => "shout",
        }
    }
}

struct Training {
    class: String,
    pcm: Vec<f32>,
}

/// Сумма вероятностей личной модели по оценкам за текущую фразу.
struct PhraseSound {
    id: u64,
    post_sum: [f32; 6],
    tags: u32,
}

/// Что известно о фразе, пока распознаются её слова.
struct PhraseStats {
    voiced_s: f32,
    ac: Option<Acoustic>,
    ended: Instant,
}

enum AsrState {
    Loading,
    Ready,
    Error(String),
}

struct Worker {
    cfg: Config,
    hub: SharedHub,
    tagger: Option<Tagger>,
    me: Thread,

    takes: Vec<Take>,
    model: Option<Model>,
    report: Report,
    building: Option<Receiver<(Option<Model>, Report)>>,
    rebuild: bool,
    /// Сглаженные вероятности личной модели по `voice::CLASSES`.
    post: [f32; 6],
    last_tag: Instant,
    guess: Vec<(String, f32)>,
    familiar: f32,
    net_shout_until: Instant,

    asr: Option<Asr>,
    asr_state: AsrState,
    lexicon: Lexicon,
    phrases: PhraseBuilder,
    phrase_sound: Option<PhraseSound>,
    pending: BTreeMap<u64, PhraseStats>,
    matcher: Matcher,
    counter: Counter,
    swear_last: Option<serde_json::Value>,
    day_check: Instant,
    /// Фраза, во время которой звук включил текущую эмоцию.
    said: Option<serde_json::Value>,

    source: Option<(rtrb::Consumer<f32>, u32)>,
    frame: Vec<f32>,
    frame_len: usize,
    gain: f32,
    resampler: Option<LinearResampler>,
    ring: VecDeque<f32>,

    db: f32,
    smooth: f32,
    loud_since: Option<Instant>,
    talk_until: Instant,
    shout_until: Instant,
    voice: Voice,
    active_until: Instant,
    frames_since_tag: u32,

    hits: BTreeMap<&'static str, VecDeque<bool>>,
    levels: BTreeMap<&'static str, f32>,
    heard: Vec<(String, f32)>,

    manual: Option<(String, Instant)>,
    auto: Option<(String, Instant)>,
    eat_until: Instant,
    don_db: f32,

    training: Option<Training>,

    sent_state: String,
    sent_at: Instant,
    meter_at: Instant,
}

impl Worker {
    fn new(cfg: Config, hub: SharedHub) -> Worker {
        let now = Instant::now();
        let gain = mic_gain(&cfg);
        let lexicon = Lexicon::new(&cfg.words);
        let matcher = Matcher::new(&cfg.swears.extra_words, &cfg.swears.ignore_words);
        let counter = Counter::new(&cfg.swears);
        let w = Worker {
            cfg,
            hub,
            tagger: None,
            me: thread::current(),
            takes: Vec::new(),
            model: None,
            report: Report::default(),
            building: None,
            rebuild: false,
            post: [0.0; 6],
            last_tag: now,
            guess: Vec::new(),
            familiar: 0.0,
            net_shout_until: now,
            asr: None,
            asr_state: AsrState::Loading,
            lexicon,
            phrases: PhraseBuilder::default(),
            phrase_sound: None,
            pending: BTreeMap::new(),
            matcher,
            counter,
            swear_last: None,
            day_check: now,
            said: None,
            source: None,
            frame: Vec::new(),
            frame_len: 480,
            gain,
            resampler: None,
            ring: VecDeque::with_capacity(WINDOW * 2),
            db: -100.0,
            smooth: -100.0,
            loud_since: None,
            talk_until: now,
            shout_until: now,
            voice: Voice::Quiet,
            active_until: now,
            frames_since_tag: 0,
            hits: EMOTIONS.iter().map(|e| (*e, VecDeque::new())).collect(),
            levels: EMOTIONS.iter().chain(&["shout"]).map(|e| (*e, 0.0)).collect(),
            heard: Vec::new(),
            manual: None,
            auto: None,
            eat_until: now,
            don_db: -100.0,
            training: None,
            sent_state: String::new(),
            sent_at: now,
            meter_at: now,
        };
        w.publish_train(None);
        w.publish_swears();
        w
    }

    fn run(&mut self, rx: Receiver<Ctrl>) {
        loop {
            loop {
                match rx.try_recv() {
                    Ok(c) => self.on_ctrl(c),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return,
                }
            }
            self.check_build();
            self.check_asr();
            self.pump_audio();
            let now = Instant::now();
            // Режим «каждый день»: новые сутки наступают и без речи.
            if now - self.day_check >= Duration::from_secs(30) {
                self.day_check = now;
                if self.cfg.swears.on && self.counter.roll_day() {
                    self.publish_swears();
                }
            }
            self.publish(now);
            // Пока идёт захват, аудиоколбэк будит нас ~40 раз в секунду. Без звука просыпаемся
            // редко: только чтобы снять истёкшую эмоцию и обновить полоски на странице настроек.
            let wait = if self.source.is_some()
                || self.hub.settings_open()
                || self.has_deadline(now)
                || self.building.is_some()
            {
                Duration::from_millis(100)
            } else {
                Duration::from_secs(5)
            };
            thread::park_timeout(wait);
        }
    }

    fn has_deadline(&self, now: Instant) -> bool {
        self.eat_until > now
            || self.manual.is_some()
            || self.auto.as_ref().is_some_and(|(_, t)| *t > now)
    }

    fn on_ctrl(&mut self, c: Ctrl) {
        let now = Instant::now();
        match c {
            Ctrl::Source { consumer, rate } => {
                self.frame_len = (rate * FRAME_MS / 1000).max(1) as usize;
                self.frame.clear();
                self.ring.clear();
                self.resampler = (rate != tagger::RATE as u32)
                    .then(|| LinearResampler::create(rate as i32, tagger::RATE))
                    .flatten();
                self.source = Some((consumer, rate));
            }
            Ctrl::Stop => {
                self.source = None;
                // Договорённая фраза распознаётся: мат в ней не теряется.
                if let Some(d) = self.phrases.flush() {
                    self.send_phrase(d);
                }
                self.db = -100.0;
                self.voice = Voice::Quiet;
                if self.training.take().is_some() {
                    self.train_msg("error", "Микрофон остановлен");
                }
            }
            Ctrl::Config(cfg) => {
                self.cfg = *cfg;
                self.gain = mic_gain(&self.cfg);
                self.lexicon = Lexicon::new(&self.cfg.words);
                let sw = &self.cfg.swears;
                self.matcher = Matcher::new(&sw.extra_words, &sw.ignore_words);
                self.counter.apply_config(sw);
                self.publish_swears();
            }
            Ctrl::SwearReset => {
                self.counter.reset();
                self.swear_last = None;
                self.publish_swears();
            }
            Ctrl::StreamStarted => {
                if self.cfg.swears.on && self.counter.on_stream_started() {
                    self.swear_last = None;
                    self.publish_swears();
                }
            }
            Ctrl::Manual(e) => match e.as_str() {
                "none" => {
                    self.manual = None;
                    self.auto = None;
                    self.eat_until = now;
                }
                "eat" => self.eat_until = now + secs(self.cfg.eat_seconds),
                _ => {
                    let s = self.cfg.emo(&e).seconds.max(4.0);
                    self.manual = Some((e, now + secs(s)));
                }
            },
            Ctrl::DonationDb(db) => {
                self.don_db = db;
                if !self.cfg.donation_source.is_empty() && db > self.cfg.donation_db {
                    // Начало алерта — ест минимум eat_seconds; пока звучит озвучка — продлеваем.
                    self.eat_until = if self.eat_until > now {
                        self.eat_until.max(now + secs(2.0))
                    } else {
                        now + secs(self.cfg.eat_seconds)
                    };
                }
            }
            Ctrl::Train(class) => {
                let Some(ci) = voice::class_index(&class) else {
                    return;
                };
                if !voice::ACOUSTIC.contains(&ci) {
                    self.train_msg("error", "Грусть и удивление узнаются по словам — записывать не нужно");
                } else if self.tagger.is_none() {
                    self.train_msg("error", "Модель звуков не загружена");
                } else if self.source.is_none() {
                    self.train_msg("error", "Микрофон не запущен");
                } else {
                    self.training = Some(Training {
                        class: class.clone(),
                        pcm: Vec::new(),
                    });
                    self.train_msg("rec", &class);
                }
            }
            Ctrl::Untrain(arg) => {
                let (class, last) = match arg.strip_suffix("#last") {
                    Some(c) => (c.to_string(), true),
                    None => (arg, false),
                };
                if let Some(ci) = voice::class_index(&class) {
                    let idx: Vec<usize> = (0..self.takes.len())
                        .filter(|&i| self.takes[i].class == ci)
                        .collect();
                    let remove: Vec<usize> = if last {
                        idx.last().copied().into_iter().collect()
                    } else {
                        idx
                    };
                    for &i in remove.iter().rev() {
                        let t = self.takes.remove(i);
                        voice::remove_take(&t.path);
                    }
                    self.start_build();
                }
            }
        }
    }

    fn pump_audio(&mut self) {
        let Some((cons, _)) = self.source.as_mut() else {
            return;
        };
        let n = cons.slots();
        if n == 0 {
            return;
        }
        let Ok(chunk) = cons.read_chunk(n) else {
            return;
        };
        let (a, b) = chunk.as_slices();
        let mut data = Vec::with_capacity(n);
        data.extend_from_slice(a);
        data.extend_from_slice(b);
        chunk.commit_all();
        for s in data {
            self.frame.push((s * self.gain).clamp(-1.0, 1.0));
            if self.frame.len() >= self.frame_len {
                let frame = std::mem::take(&mut self.frame);
                self.on_frame(&frame);
                self.frame = frame;
                self.frame.clear();
            }
        }
    }

    fn on_frame(&mut self, frame: &[f32]) {
        let now = Instant::now();
        let rms = (frame.iter().map(|x| x * x).sum::<f32>() / frame.len() as f32).sqrt();
        self.db = if rms > 1e-5 {
            (20.0 * rms.log10()).max(-100.0)
        } else {
            -100.0
        };
        self.update_voice(now);

        let pcm16 = match &self.resampler {
            Some(r) => r.resample(frame, false),
            None => frame.to_vec(),
        };
        self.ring.extend(pcm16.iter());
        let extra = self.ring.len().saturating_sub(WINDOW * 2);
        self.ring.drain(..extra);
        self.track_phrase(&pcm16);

        if let Some(t) = self.training.as_mut() {
            t.pcm.extend_from_slice(&pcm16);
            if t.pcm.len() as f32 >= train_seconds(&t.class) * tagger::RATE as f32 {
                let t = self.training.take().unwrap();
                self.finish_training(t);
            }
        }

        // «Активность» чуть ниже порога речи: вздох и смешок бывают тише слов.
        if self.db > self.cfg.talk_db - 6.0 {
            self.active_until = now + Duration::from_millis(400);
        }
        self.frames_since_tag += 1;
        if self.frames_since_tag >= TAG_EVERY_FRAMES {
            self.frames_since_tag = 0;
            self.tag(now);
        }
    }

    /// Рот: говорит / молчит — по громкости. Крик — нейросетью, если крик записан в обучении
    /// (громкость после компрессора и лимитера ничего не говорит). Иначе — по громкости:
    /// громко держится ~0,15 с, чтобы отдельные «п» и «б» его не включали.
    fn update_voice(&mut self, now: Instant) {
        let k = if self.db > self.smooth { 0.35 } else { 0.08 };
        self.smooth += (self.db - self.smooth) * k;
        let net_shout = self.net_shout();
        if !net_shout && self.smooth > self.cfg.shout_db {
            let since = *self.loud_since.get_or_insert(now);
            if now - since >= Duration::from_millis(150) {
                self.shout_until = now + Duration::from_millis(400);
            }
        } else {
            self.loud_since = None;
        }
        if self.db > self.cfg.talk_db {
            self.talk_until = now + Duration::from_millis(120);
        }
        let talking = now < self.talk_until;
        let shouting = if net_shout {
            talking && now < self.net_shout_until
        } else {
            now < self.shout_until
        };
        self.voice = if shouting {
            Voice::Shout
        } else if talking {
            Voice::Talk
        } else {
            Voice::Quiet
        };
    }

    fn net_shout(&self) -> bool {
        self.model.as_ref().is_some_and(|m| m.has(voice::SHOUT))
    }

    fn reset_scores(&mut self) {
        for e in EMOTIONS {
            push_hit(self.hits.get_mut(e).unwrap(), false);
        }
        for v in self.levels.values_mut() {
            *v = 0.0;
        }
        self.post = [0.0; 6];
        self.guess.clear();
    }

    fn tag(&mut self, now: Instant) {
        let any_on = EMOTIONS.iter().any(|e| self.cfg.emo(e).on) || self.net_shout();
        let wanted = any_on || self.hub.settings_open();
        let Some(tagger) = self.tagger.as_ref() else {
            return;
        };
        if !wanted || now > self.active_until || self.ring.len() < WINDOW {
            // Тишина: модель не трогаем, полоски гаснут.
            self.reset_scores();
            return;
        }
        let clip: Vec<f32> = self.ring.iter().skip(self.ring.len() - WINDOW).copied().collect();
        let probs = tagger.probs(&clip);
        let generic = tagger.group_scores(&probs);
        self.heard = top_classes(&tagger.names, &probs, 3);

        // Личная модель: вероятности сглаживаются по соседним окнам (0,2 с).
        if let Some(m) = &self.model {
            let (p, fam) = m.predict_live(&voice::features(&probs, &clip));
            self.familiar = fam;
            let fresh = now - self.last_tag > Duration::from_millis(1200);
            for (s, x) in self.post.iter_mut().zip(p) {
                *s = if fresh { x } else { 0.5 * *s + 0.5 * x };
            }
            let mut g: Vec<(String, f32)> = m
                .classes
                .iter()
                .map(|&c| (voice::CLASSES[c].to_string(), (self.post[c] * 100.0).round() / 100.0))
                .collect();
            g.sort_by(|a, b| b.1.total_cmp(&a.1));
            g.truncate(2);
            self.guess = g;
            if let Some(id) = self.phrases.current() {
                let ps = match &mut self.phrase_sound {
                    Some(ps) if ps.id == id => ps,
                    slot => slot.insert(PhraseSound {
                        id,
                        post_sum: [0.0; 6],
                        tags: 0,
                    }),
                };
                for (s, x) in ps.post_sum.iter_mut().zip(self.post) {
                    *s += x;
                }
                ps.tags += 1;
            }
        }
        self.last_tag = now;
        let by_words = matches!(self.asr_state, AsrState::Ready);

        if self.net_shout() {
            let level = self.post[voice::SHOUT] / post_thr(self.cfg.shout_sens);
            self.levels.insert("shout", level);
            if level >= 1.0 {
                self.net_shout_until = now + NET_SHOUT_HOLD;
            }
        }

        let mut best: Option<(&'static str, f32)> = None;
        for e in EMOTIONS {
            let ec = self.cfg.emo(e);
            let ci = voice::class_index(e).unwrap();
            if by_words && matches!(e, "sad" | "surprise") {
                // Только по словам: звук их от обычной речи не отличает.
                self.levels.insert(e, 0.0);
                push_hit(self.hits.get_mut(e).unwrap(), false);
                continue;
            }
            let trained = self.model.as_ref().is_some_and(|m| m.has(ci));
            let level = if trained {
                self.post[ci] / post_thr(ec.sens)
            } else {
                let sens = ec.sens.clamp(1, 10) as f32;
                generic[e] / (0.55 - (sens - 1.0) * 0.047)
            };
            self.levels.insert(e, level);
            let hits = self.hits.get_mut(e).unwrap();
            push_hit(hits, ec.on && level >= 1.0);
            let need = if e == "sing" { 3 } else { 2 };
            if hits.iter().filter(|h| **h).count() >= need && best.is_none_or(|(_, l)| level > l) {
                best = Some((e, level));
            }
        }
        if let Some((e, _)) = best {
            self.set_auto(e, now);
        }
    }

    fn set_auto(&mut self, e: &str, now: Instant) {
        let until = now + secs(self.cfg.emo(e).seconds);
        match &mut self.auto {
            // та же эмоция продолжается — продлеваем
            Some((cur, t)) if cur == e && *t > now => *t = (*t).max(until),
            _ => self.auto = Some((e.to_string(), until)),
        }
    }

    // ---------------------------------------------------------------------------------------
    // Слова

    /// Нужны ли слова: включён счётчик мата или эмоция, которую они различают, или открыты
    /// настройки.
    fn words_wanted(&self) -> bool {
        matches!(self.asr_state, AsrState::Ready)
            && (self.cfg.swears.on
                || ["laugh", "sad", "surprise", "sing"].iter().any(|e| self.cfg.emo(e).on)
                || self.hub.settings_open())
    }

    fn track_phrase(&mut self, pcm16: &[f32]) {
        if !self.words_wanted() {
            if self.phrases.current().is_some() {
                self.phrases.flush();
            }
            return;
        }
        let active = self.db > self.cfg.talk_db - 6.0;
        if let Some(d) = self.phrases.push(pcm16, active) {
            self.send_phrase(d);
        }
    }

    /// Готовая фраза — в распознавание речи.
    fn send_phrase(&mut self, d: crate::phrase::Done) {
        let Some(asr) = self.asr.as_ref() else {
            return;
        };
        let sound = self.phrase_sound.take().filter(|ps| ps.id == d.id && ps.tags > 0);
        let ac = match (&self.model, sound) {
            (Some(m), Some(ps)) => {
                let mut mean = ps.post_sum;
                mean.iter_mut().for_each(|v| *v /= ps.tags as f32);
                Some(Acoustic {
                    mean,
                    has_laugh: m.has(2),
                })
            }
            _ => None,
        };
        self.pending.insert(
            d.id,
            PhraseStats {
                voiced_s: d.voiced_s,
                ac,
                ended: Instant::now(),
            },
        );
        while self.pending.len() > PENDING_MAX {
            self.pending.pop_first();
        }
        asr.send(d.id, d.pcm);
    }

    fn check_asr(&mut self) {
        let Some(asr) = self.asr.as_ref() else {
            return;
        };
        let msgs: Vec<AsrMsg> = asr.rx.try_iter().collect();
        for m in msgs {
            match m {
                AsrMsg::Ready(Ok(())) => self.asr_state = AsrState::Ready,
                AsrMsg::Ready(Err(e)) => {
                    self.asr_state = AsrState::Error(e);
                    self.asr = None;
                    return;
                }
                AsrMsg::Text { id, text, ms } => self.on_words(id, text, ms),
            }
        }
    }

    fn on_words(&mut self, id: u64, text: String, ms: f32) {
        let Some(st) = self.pending.remove(&id) else {
            return;
        };
        if self.cfg.swears.on && !text.is_empty() {
            let n = self.matcher.count(&text);
            if n > 0 {
                self.counter.add(n);
                self.swear_last = Some(json!({ "text": text, "n": n }));
                self.publish_swears();
            }
        }
        let now = Instant::now();
        let toks = words::tokens(&text);
        let counts = self.lexicon.count(&toks);
        let fresh = now - st.ended < WORDS_LATE && st.voiced_s >= EMO_MIN_VOICED;
        let decided = words::decide(counts, words::word_count(&toks), st.voiced_s, st.ac.as_ref())
            .filter(|_| fresh);
        let e = decided.filter(|e| self.cfg.emo(e).on);
        if let Some(e) = e {
            self.set_auto(e, now);
            self.levels.insert(e, 1.0);
        }
        self.said = Some(json!({ "text": text, "e": e, "ms": ms.round() }));
    }

    fn publish_swears(&self) {
        let sw = &self.cfg.swears;
        let msg = json!({
            "t": "swears",
            "on": sw.on,
            "count": self.counter.count(),
            "file": self.counter.file().display().to_string(),
            "last": self.swear_last,
        });
        self.hub.sticky("swears", msg.to_string());
    }

    // ---------------------------------------------------------------------------------------
    // Обучение

    fn load_voice(&mut self) {
        let Some(t) = self.tagger.as_ref() else {
            return;
        };
        self.takes = voice::load_takes(t);
        self.start_build();
    }

    /// Переобучить модель в отдельном потоке (~0,5–3 с на всех записях).
    fn start_build(&mut self) {
        if self.building.is_some() {
            self.rebuild = true;
            self.publish_train(None);
            return;
        }
        let data = voice::take_data(&self.takes);
        let (tx, rx) = mpsc::channel();
        let me = self.me.clone();
        let spawned = thread::Builder::new().name("voice-train".into()).spawn(move || {
            win::background_current_thread();
            let _ = tx.send(voice::build(&data));
            me.unpark();
        });
        if spawned.is_ok() {
            self.building = Some(rx);
        }
        self.publish_train(None);
    }

    fn check_build(&mut self) {
        let Some(rx) = self.building.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok((model, report)) => {
                self.building = None;
                self.model = model;
                self.report = report;
                self.post = [0.0; 6];
                if std::mem::take(&mut self.rebuild) {
                    self.start_build();
                } else {
                    self.publish_train(None);
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => self.building = None,
        }
    }

    fn finish_training(&mut self, t: Training) {
        let Some(tagger) = self.tagger.as_ref() else {
            return;
        };
        let Some(class) = voice::class_index(&t.class) else {
            return;
        };
        let peak = t
            .pcm
            .chunks(tagger::RATE as usize / 10)
            .map(|f| (f.iter().map(|x| x * x).sum::<f32>() / f.len() as f32).sqrt())
            .fold(0.0f32, f32::max);
        if peak < 0.003 {
            self.train_msg("error", "Слишком тихо — ничего не услышала. Попробуй ещё раз ближе к микрофону");
            return;
        }
        let (feats, aug, mean) = voice::take_features(tagger, &t.pcm);
        if feats.is_empty() {
            self.train_msg("error", "Не нашла голос в записи, попробуй ещё раз");
            return;
        }
        let heard = top_classes(&tagger.names, &mean, 3);
        // Что про эту запись думает модель, обученная до неё.
        let guess = self.model.as_ref().map(|m| {
            let p = m.predict_all(&feats);
            let c = (0..6).max_by(|a, b| p[*a].total_cmp(&p[*b])).unwrap_or(0);
            json!({ "c": voice::CLASSES[c], "p": (p[c] * 100.0).round() / 100.0 })
        });
        let path = match voice::save_take(class, &t.pcm) {
            Ok(p) => p,
            Err(e) => {
                self.train_msg("error", &format!("Не удалось сохранить запись: {e}"));
                return;
            }
        };
        voice::write_cache(&path, voice::cache_ver(tagger), &feats, &aug);
        self.takes.push(Take {
            class,
            path,
            feats,
            aug,
        });
        // Старые записи класса сверх лимита — удаляем.
        while self.takes.iter().filter(|t| t.class == class).count() > voice::MAX_TAKES {
            if let Some(i) = self.takes.iter().position(|t| t.class == class) {
                let old = self.takes.remove(i);
                voice::remove_take(&old.path);
            }
        }
        let done = json!({ "e": t.class, "heard": heard, "guess": guess });
        self.start_build();
        self.publish_train(Some(done));
    }

    fn train_msg(&self, state: &str, text: &str) {
        let msg = json!({ "t": "training", "state": state, "text": text });
        self.hub.send(true, &msg.to_string());
    }

    fn publish_train(&self, done: Option<serde_json::Value>) {
        let r = &self.report;
        let mut counts = BTreeMap::new();
        let mut acc = BTreeMap::new();
        let mut confused = BTreeMap::new();
        for (c, name) in voice::CLASSES.iter().enumerate() {
            counts.insert(*name, self.takes.iter().filter(|t| t.class == c).count());
            acc.insert(*name, r.acc[c].map(|a| (a * 100.0).round()));
            confused.insert(*name, r.confused[c].map(|o| voice::CLASSES[o]));
        }
        let msg = json!({
            "t": "train",
            "counts": counts,
            "acc": acc,
            "confused": confused,
            "overall": r.overall.map(|a| (a * 100.0).round()),
            "classes": self.model.as_ref().map(|m| m.classes.iter().map(|&c| voice::CLASSES[c]).collect::<Vec<_>>()),
            "busy": self.building.is_some(),
            "done": done,
        });
        self.hub.sticky("train", msg.to_string());
    }

    fn emotion_now(&mut self, now: Instant) -> String {
        if self.manual.as_ref().is_some_and(|(_, t)| *t <= now) {
            self.manual = None;
        }
        if self.eat_until > now {
            return "eat".into();
        }
        if let Some((e, _)) = &self.manual {
            return e.clone();
        }
        match &self.auto {
            Some((e, t)) if *t > now => e.clone(),
            _ => "none".into(),
        }
    }

    fn publish(&mut self, now: Instant) {
        let emo = self.emotion_now(now);
        let level = if self.voice == Voice::Quiet {
            0.0
        } else {
            ((self.db - self.cfg.talk_db) / -self.cfg.talk_db).clamp(0.0, 1.0)
        };
        let level = (level * 20.0).round() / 20.0;
        let state = json!({ "t": "state", "v": self.voice.name(), "l": level, "e": emo }).to_string();
        // Голос и эмоция — сразу; одна только громкость — не чаще 20 раз в секунду
        // (каждое изменение — новый кадр в OBS).
        let major_change = {
            let old: serde_json::Value = serde_json::from_str(&self.sent_state).unwrap_or_default();
            old["v"] != self.voice.name() || old["e"] != emo.as_str()
        };
        if state != self.sent_state
            && (major_change || now - self.sent_at >= Duration::from_millis(50))
        {
            self.hub.sticky("state", state.clone());
            self.sent_state = state;
            self.sent_at = now;
        }
        if self.hub.settings_open() && now - self.meter_at >= Duration::from_millis(80) {
            self.meter_at = now;
            let r = |x: f32| (x * 100.0).round() / 100.0;
            let msg = json!({
                "t": "m",
                "db": r(self.db),
                "v": self.voice.name(),
                "e": emo,
                "don": r(self.don_db),
                "sc": self.levels.iter().map(|(k, v)| (*k, r(*v))).collect::<BTreeMap<_, _>>(),
                "heard": self.heard,
                "guess": self.guess,
                "fam": r(self.familiar),
                "net": self.net_shout(),
                "said": self.said,
                "asr": match &self.asr_state {
                    AsrState::Loading => "loading".to_string(),
                    AsrState::Ready => "ready".to_string(),
                    AsrState::Error(e) => e.clone(),
                },
            });
            self.hub.send(true, &msg.to_string());
        }
    }
}

fn mic_gain(cfg: &Config) -> f32 {
    10f32.powf(cfg.mic_gain_db.clamp(0.0, 40.0) / 20.0)
}

fn push_hit(h: &mut VecDeque<bool>, v: bool) {
    h.push_back(v);
    while h.len() > 3 {
        h.pop_front();
    }
}

fn secs(s: f32) -> Duration {
    Duration::from_secs_f32(s.max(0.1))
}

/// Три самых вероятных класса AudioSet — показываем на странице, что «слышит» модель.
fn top_classes(names: &[String], probs: &[f32], k: usize) -> Vec<(String, f32)> {
    let mut idx: Vec<usize> = (0..probs.len()).collect();
    idx.sort_by(|a, b| probs[*b].total_cmp(&probs[*a]));
    idx.into_iter()
        .take(k)
        .filter(|&i| probs[i] >= 0.05)
        .map(|i| (names.get(i).cloned().unwrap_or_default(), (probs[i] * 100.0).round() / 100.0))
        .collect()
}

/// Сколько секунд в записи звучит голос (кадры 10 мс не тише пика на 25 дБ).
fn voiced_seconds(pcm: &[f32]) -> f32 {
    let db: Vec<f32> = pcm
        .chunks(tagger::RATE as usize / 100)
        .map(|f| {
            let rms = (f.iter().map(|x| x * x).sum::<f32>() / f.len() as f32).sqrt();
            20.0 * rms.max(1e-6).log10()
        })
        .collect();
    let peak = db.iter().copied().fold(-120.0f32, f32::max);
    let thr = (peak - 25.0).max(-60.0);
    db.iter().filter(|d| **d > thr).count() as f32 / 100.0
}

/// Прогон WAV-файла (для проверки): `vox.exe --wav file.wav`.
pub fn run_wav(path: &str) -> Result<(), String> {
    let dir = Config::model_dir();
    let tagger = Tagger::load(&dir.join(SOUND_MODEL))?;
    let (pcm, rate) = voice::read_wav(std::path::Path::new(path))?;
    let pcm = if rate != tagger::RATE as u32 {
        LinearResampler::create(rate as i32, tagger::RATE)
            .ok_or("resampler")?
            .resample(&pcm, true)
    } else {
        pcm
    };
    let takes = voice::load_takes(&tagger);
    let data = voice::take_data(&takes);
    let model = voice::build(&data).0;
    let mut sum = [0.0f32; 6];
    let mut n = 0;
    let mut i = 0;
    while i + WINDOW <= pcm.len().max(WINDOW) {
        let end = (i + WINDOW).min(pcm.len());
        let t0 = Instant::now();
        let probs = tagger.probs(&pcm[i..end]);
        let ms = t0.elapsed().as_secs_f32() * 1000.0;
        let g = tagger.group_scores(&probs);
        if let Some(m) = &model {
            let (p, fam) = m.predict_live(&voice::features(&probs, &pcm[i..end]));
            for (s, x) in sum.iter_mut().zip(p) {
                *s += x;
            }
            n += 1;
            print!("[знакомо {:.0}%] ", fam * 100.0);
            let mut v: Vec<(usize, f32)> = m.classes.iter().map(|&c| (c, p[c])).collect();
            v.sort_by(|a, b| b.1.total_cmp(&a.1));
            print!("[личная: {} {:.0}%, {} {:.0}%]  ", voice::CLASSES[v[0].0], v[0].1 * 100.0, voice::CLASSES[v[1].0], v[1].1 * 100.0);
        }
        println!(
            "{:5.1}s  {:5.1} мс  {:?}  {:?}",
            i as f32 / tagger::RATE as f32,
            ms,
            g.iter().map(|(k, v)| format!("{k}={v:.2}")).collect::<Vec<_>>(),
            top_classes(&tagger.names, &probs, 3)
        );
        i += tagger::RATE as usize / 2;
        if end == pcm.len() {
            break;
        }
    }
    let rec = asr::Recognizer::load(&dir.join(WORDS_MODEL))?;
    let t0 = Instant::now();
    let text = rec.decode(&pcm);
    let ms = t0.elapsed().as_secs_f32() * 1000.0;
    let toks = words::tokens(&text);
    let counts = Lexicon::new(&Config::load().0.words).count(&toks);
    let ac = model.as_ref().filter(|_| n > 0).map(|m| Acoustic {
        mean: sum.map(|v| v / n as f32),
        has_laugh: m.has(2),
    });
    let d = words::decide(counts, words::word_count(&toks), voiced_seconds(&pcm), ac.as_ref());
    println!("слова ({ms:.0} мс): «{text}»  удивление/грусть/смех: {counts:?}  →  {}", d.unwrap_or("—"));
    Ok(())
}

/// Проверка на записях пользователя: `vox.exe --eval`. Каждая запись оценивается
/// моделью, которая её не видела.
pub fn run_eval() -> Result<(), String> {
    let dir = Config::model_dir();
    let ced = std::env::var("VOX_CED").unwrap_or_else(|_| SOUND_MODEL.into());
    let tagger = Tagger::load(&dir.join(&ced))?;
    {
        let pcm: Vec<f32> = (0..voice::WIN).map(|i| ((i as f32) * 0.05).sin() * 0.2).collect();
        let t0 = Instant::now();
        for _ in 0..10 {
            std::hint::black_box(tagger.probs(&pcm));
        }
        let t_ced = t0.elapsed().as_secs_f32() * 100.0;
        let t0 = Instant::now();
        for _ in 0..10 {
            std::hint::black_box(crate::prosody::features(&pcm));
        }
        println!("{ced}: {t_ced:.1} мс на окно 2 с, голосовые признаки {:.1} мс", t0.elapsed().as_secs_f32() * 100.0);
    }
    let t0 = Instant::now();
    let takes = voice::load_takes(&tagger);
    println!("записей: {}, признаки за {:.1} с", takes.len(), t0.elapsed().as_secs_f32());
    let data = voice::take_data(&takes);
    let t0 = Instant::now();
    let (model, rep) = voice::build(&data);
    println!("обучение + проверка за {:.1} с", t0.elapsed().as_secs_f32());
    if let Some(model) = &model {
        println!("\nзвук, по окнам 2 с:");
        print!("{:>9}", "");
        for &c in &model.classes {
            print!("{:>9}", voice::CLASSES[c]);
        }
        println!("   записей  точность");
        for &c in &model.classes {
            print!("{:>9}", voice::CLASSES[c]);
            for &o in &model.classes {
                print!("{:>8.0}%", rep.confusion[c][o] * 100.0);
            }
            println!("{:>10}{:>9.0}%", rep.takes[c], rep.acc[c].unwrap_or(0.0) * 100.0);
        }
        println!("средняя точность: {:.0}%", rep.overall.unwrap_or(0.0) * 100.0);
    } else {
        println!("звуковой модели нет: нужно минимум {} записи «обычной речи» и ещё одного класса", voice::MIN_TAKES);
    }

    let words_model = std::env::var("VOX_WORDS").unwrap_or_else(|_| WORDS_MODEL.into());
    let rec = asr::Recognizer::load(&dir.join(&words_model))?;
    let lex = Lexicon::new(&Config::load().0.words);
    let classes = voice::model_classes(&data);
    let folds = voice::folds(&data);
    let fold_models: Vec<voice::Model> = if model.is_some() {
        (0..voice::FOLDS)
            .map(|f| voice::fit(&data, &classes, &|ti| folds[ti] != f))
            .collect()
    } else {
        Vec::new()
    };
    println!("\nзвук + слова, по целой записи:");
    let (mut right, mut total) = ([0usize; 6], [0usize; 6]);
    let mut asr_ms = 0.0;
    for (ti, t) in takes.iter().enumerate() {
        let m = if classes.contains(&t.class) {
            fold_models.get(folds[ti])
        } else {
            model.as_ref()
        };
        let ac = m.map(|m| {
            let mut mean = [0.0f32; 6];
            for x in &t.feats {
                let (p, _) = m.predict_live(x);
                for (s, v) in mean.iter_mut().zip(p) {
                    *s += v / t.feats.len() as f32;
                }
            }
            Acoustic {
                mean,
                has_laugh: m.has(2),
            }
        });
        let (pcm, _) = voice::read_wav(&t.path)?;
        let t0 = Instant::now();
        let text = rec.decode(&pcm);
        asr_ms += t0.elapsed().as_secs_f32() * 1000.0 / (pcm.len() as f32 / tagger::RATE as f32);
        let toks = words::tokens(&text);
        let counts = lex.count(&toks);
        let shout = ac.as_ref().is_some_and(|a| {
            (0..6).max_by(|x, y| a.mean[*x].total_cmp(&a.mean[*y])) == Some(voice::SHOUT)
        });
        let out = words::decide(counts, words::word_count(&toks), voiced_seconds(&pcm), ac.as_ref())
            .unwrap_or(if shout { "shout" } else { "neutral" });
        let truth = voice::CLASSES[t.class];
        total[t.class] += 1;
        if out == truth {
            right[t.class] += 1;
        }
        let snd = ac
            .as_ref()
            .map(|a| {
                voice::ACOUSTIC
                    .iter()
                    .map(|&c| format!("{}={:.2}", &voice::CLASSES[c][..4], a.mean[c]))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        println!(
            "{} {:>9} → {:<9} {}  «{}»",
            if out == truth { "✓" } else { "✗" },
            truth,
            out,
            snd,
            text
        );
    }
    let accs: Vec<f32> = (0..6)
        .filter(|&c| total[c] > 0)
        .map(|c| right[c] as f32 / total[c] as f32)
        .collect();
    for c in (0..6).filter(|&c| total[c] > 0) {
        print!("{} {}/{}  ", voice::CLASSES[c], right[c], total[c]);
    }
    println!(
        "\nсредняя точность: {:.0}%  (слова: {:.0} мс на секунду звука)",
        accs.iter().sum::<f32>() / accs.len().max(1) as f32 * 100.0,
        asr_ms / takes.len().max(1) as f32
    );
    Ok(())
}

/// Счёт мата на файле (проверка моделей): `vox.exe --swears файл.wav`. Фразы режутся так же,
/// как вживую; голос — по уровню выше шума (как в Солонке). Модель слов: `VOX_WORDS`.
pub fn run_swears(path: &str) -> Result<(), String> {
    let dir = Config::model_dir();
    let model = std::env::var("VOX_WORDS").unwrap_or_else(|_| WORDS_MODEL.into());
    let rec = asr::Recognizer::load(&dir.join(&model))?;
    let (pcm, rate) = voice::read_wav(std::path::Path::new(path))?;
    let pcm = if rate != tagger::RATE as u32 {
        LinearResampler::create(rate as i32, tagger::RATE)
            .ok_or("resampler")?
            .resample(&pcm, true)
    } else {
        pcm
    };
    let m = crate::matcher::Matcher::new(&[], &[]);
    let mut b = crate::phrase::PhraseBuilder::default();
    let (mut noise, mut total, mut spent) = (0.004f32, 0u32, 0.0f32);
    let run = |d: crate::phrase::Done, total: &mut u32, spent: &mut f32| {
        let t0 = Instant::now();
        let text = rec.decode(&d.pcm);
        *spent += t0.elapsed().as_secs_f32();
        let n = m.count(&text);
        *total += n;
        println!("+{n}  «{text}»");
    };
    for f in pcm.chunks(tagger::RATE as usize / 100) {
        let rms = (f.iter().map(|x| x * x).sum::<f32>() / f.len() as f32).sqrt();
        let active = rms > 0.004f32.max(noise * 3.0);
        if !active && b.current().is_none() {
            noise += (rms - noise) * if rms < noise { 0.1 } else { 0.005 };
        }
        if let Some(d) = b.push(f, active) {
            run(d, &mut total, &mut spent);
        }
    }
    if let Some(d) = b.flush() {
        run(d, &mut total, &mut spent);
    }
    let audio = pcm.len() as f32 / tagger::RATE as f32;
    println!("{model}: матов {total}; {audio:.1} с звука распознано за {:.2} с ({:.0} мс на секунду)", spent, spent * 1000.0 / audio);
    Ok(())
}
