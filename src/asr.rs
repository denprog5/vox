//! Распознавание слов (Zipformer, русский) по законченным фразам — в отдельном потоке
//! с фоновым приоритетом: фраза в 2 с распознаётся за ~30 мс одного ядра, а в тишине
//! поток спит.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, Thread};
use std::time::Instant;

use sherpa_onnx::{
    OfflineNemoEncDecCtcModelConfig, OfflineRecognizer, OfflineRecognizerConfig,
    OfflineTransducerModelConfig,
};

use crate::tagger;
use crate::win;

pub struct Recognizer {
    rec: OfflineRecognizer,
}

impl Recognizer {
    pub fn load(dir: &Path) -> Result<Recognizer, String> {
        // Zipformer (encoder/decoder/joiner) или CTC-модель одним файлом (GigaAM).
        let tokens = dir.join("tokens.txt");
        let file = |names: &[&str]| names.iter().map(|n| dir.join(n)).find(|p| p.is_file());
        let mut cfg = OfflineRecognizerConfig::default();
        if let (Some(enc), Some(dec), Some(joi)) = (
            file(&["encoder.int8.onnx", "encoder.onnx"]),
            file(&["decoder.onnx", "decoder.int8.onnx"]),
            file(&["joiner.int8.onnx", "joiner.onnx"]),
        ) {
            cfg.model_config.transducer = OfflineTransducerModelConfig {
                encoder: Some(win::short_path(&enc)),
                decoder: Some(win::short_path(&dec)),
                joiner: Some(win::short_path(&joi)),
            };
        } else if let Some(model) = file(&["model.int8.onnx"]) {
            cfg.model_config.nemo_ctc = OfflineNemoEncDecCtcModelConfig {
                model: Some(win::short_path(&model)),
            };
        } else {
            return Err(format!("Нет модели слов: {}", dir.display()));
        }
        if !tokens.is_file() {
            return Err(format!("Нет модели слов: {}", dir.display()));
        }
        cfg.decoding_method = Some("greedy_search".into());
        cfg.model_config.tokens = Some(win::short_path(&tokens));
        cfg.model_config.num_threads = 1;
        cfg.model_config.provider = Some("cpu".into());
        let rec = OfflineRecognizer::create(&cfg).ok_or("Не удалось загрузить модель слов")?;
        Ok(Recognizer { rec })
    }

    /// Текст фразы (моно 16 кГц). Громкость выравнивается, как для модели звуков.
    pub fn decode(&self, pcm: &[f32]) -> String {
        let clip = tagger::normalize(pcm);
        let stream = self.rec.create_stream();
        stream.accept_waveform(tagger::RATE, &clip);
        self.rec.decode(&stream);
        stream
            .get_result()
            .map(|r| r.text.trim().to_string())
            .unwrap_or_default()
    }
}

pub enum AsrMsg {
    Ready(Result<(), String>),
    Text { id: u64, text: String, ms: f32 },
}

pub struct Asr {
    tx: Sender<(u64, Vec<f32>)>,
    pub rx: Receiver<AsrMsg>,
}

impl Asr {
    /// Загрузка модели (~1 с) идёт в потоке; `notify` будится на каждый результат.
    pub fn spawn(dir: PathBuf, notify: Thread) -> Asr {
        let (tx, jobs) = mpsc::channel::<(u64, Vec<f32>)>();
        let (out, rx) = mpsc::channel();
        let _ = thread::Builder::new().name("words".into()).spawn(move || {
            win::background_current_thread();
            let rec = match Recognizer::load(&dir) {
                Ok(r) => {
                    let _ = out.send(AsrMsg::Ready(Ok(())));
                    r
                }
                Err(e) => {
                    let _ = out.send(AsrMsg::Ready(Err(e)));
                    notify.unpark();
                    return;
                }
            };
            notify.unpark();
            // Распознаём каждую фразу: для счётчика мата важна каждая. Запоздавшую фразу
            // движок не превратит в эмоцию, но мат в ней посчитает.
            while let Ok((id, pcm)) = jobs.recv() {
                let t0 = Instant::now();
                let text = rec.decode(&pcm);
                let ms = t0.elapsed().as_secs_f32() * 1000.0;
                if out.send(AsrMsg::Text { id, text, ms }).is_err() {
                    return;
                }
                notify.unpark();
            }
        });
        Asr { tx, rx }
    }

    pub fn send(&self, id: u64, pcm: Vec<f32>) {
        let _ = self.tx.send((id, pcm));
    }
}
