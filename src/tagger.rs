//! Распознавание звуков: CED-small (AudioSet, 527 классов) через sherpa-onnx.
//!
//! Без обучения эмоция = вероятность группы классов (смех = Laughter/Giggle/…).
//! С обучением все 527 вероятностей идут в личную модель голоса (`voice.rs`).

use std::collections::BTreeMap;
use std::path::Path;

use sherpa_onnx::{AudioTagging, AudioTaggingConfig, AudioTaggingModelConfig};

use crate::win;

pub const RATE: i32 = 16_000;

/// Классы AudioSet по эмоциям (названия как в class_labels_indices.csv).
const GROUPS: [(&str, &[&str]); 4] = [
    (
        "laugh",
        &["Laughter", "Baby laughter", "Giggle", "Snicker", "Belly laugh", "Chuckle, chortle"],
    ),
    (
        "sad",
        &["Crying, sobbing", "Baby cry, infant cry", "Whimper", "Wail, moan", "Sigh", "Groan"],
    ),
    ("surprise", &["Gasp"]),
    (
        "sing",
        &[
            "Singing", "Choir", "Yodeling", "Chant", "Male singing", "Female singing",
            "Child singing", "Humming",
        ],
    ),
];

pub struct Tagger {
    at: AudioTagging,
    n: usize,
    pub names: Vec<String>,
    /// Отпечаток модели (размер файла): кэш признаков от другой модели не годится.
    pub tag: u32,
    groups: Vec<(&'static str, Vec<usize>)>,
}

impl Tagger {
    pub fn load(dir: &Path) -> Result<Tagger, String> {
        let model = dir.join("model.int8.onnx");
        let labels = dir.join("class_labels_indices.csv");
        if !model.is_file() || !labels.is_file() {
            return Err(format!("Нет модели звуков: {}", dir.display()));
        }
        let text = std::fs::read_to_string(&labels).map_err(|e| e.to_string())?;
        let names: Vec<String> = text
            .lines()
            .skip(1)
            .filter_map(|l| {
                let mut it = l.splitn(3, ',');
                it.next()?;
                it.next()?;
                Some(it.next()?.trim().trim_matches('"').to_string())
            })
            .collect();
        let groups = GROUPS
            .iter()
            .map(|(g, list)| {
                let idx = list
                    .iter()
                    .filter_map(|n| names.iter().position(|x| x == n))
                    .collect();
                (*g, idx)
            })
            .collect();
        let cfg = AudioTaggingConfig {
            model: AudioTaggingModelConfig {
                ced: Some(win::short_path(&model)),
                num_threads: 1,
                ..Default::default()
            },
            labels: Some(win::short_path(&labels)),
            top_k: names.len() as i32,
        };
        let at = AudioTagging::create(&cfg).ok_or("Не удалось загрузить модель звуков")?;
        let tag = std::fs::metadata(&model).map_or(0, |m| (m.len() & 0xff_ffff) as u32);
        Ok(Tagger {
            at,
            n: names.len(),
            names,
            tag,
            groups,
        })
    }

    /// Вероятности всех классов для клипа (моно 16 кГц). Громкость выравнивается,
    /// чтобы тихий микрофон не мешал распознаванию.
    pub fn probs(&self, pcm: &[f32]) -> Vec<f32> {
        let clip = normalize(pcm);
        let stream = self.at.create_stream();
        stream.accept_waveform(RATE, &clip);
        let mut out = vec![0.0; self.n];
        for ev in self.at.compute(&stream, self.n as i32) {
            if let Some(slot) = out.get_mut(ev.index as usize) {
                *slot = ev.prob;
            }
        }
        out
    }

    /// Общая оценка каждой эмоции: максимум вероятностей её классов.
    pub fn group_scores(&self, probs: &[f32]) -> BTreeMap<&'static str, f32> {
        self.groups
            .iter()
            .map(|(g, idx)| {
                let s = idx.iter().map(|&i| probs[i]).fold(0.0f32, f32::max);
                (*g, s)
            })
            .collect()
    }
}

/// RMS → −20 дБFS, усиление не больше +24 дБ, без клиппинга.
pub fn normalize(pcm: &[f32]) -> Vec<f32> {
    let rms = (pcm.iter().map(|x| x * x).sum::<f32>() / pcm.len().max(1) as f32).sqrt();
    let peak = pcm.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    if rms < 1e-6 {
        return pcm.to_vec();
    }
    let gain = (0.1 / rms).min(16.0).min(0.98 / peak.max(1e-6));
    pcm.iter().map(|x| x * gain).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_limits_gain_and_peak() {
        let quiet: Vec<f32> = (0..1600).map(|i| ((i as f32) * 0.1).sin() * 0.001).collect();
        let n = normalize(&quiet);
        let peak = n.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak <= 0.0161, "gain must be capped at +24 dB, peak {peak}");
        let loud: Vec<f32> = (0..1600).map(|i| ((i as f32) * 0.1).sin() * 0.9).collect();
        assert!(normalize(&loud).iter().all(|x| x.abs() <= 0.98));
    }
}
