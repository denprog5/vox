//! Настройки (`%APPDATA%\Vox\config.json`). Меняются со страницы настроек;
//! файл можно править и руками — ошибка разбора не перетирает его.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const EMOTIONS: [&str; 4] = ["laugh", "sad", "surprise", "sing"];

/// Папки моделей в `models\`: звуки (CED-small, AudioSet) и русская речь (Zipformer).
pub const SOUND_MODEL: &str = "ced-small";
pub const WORDS_MODEL: &str = "zipformer-ru";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct EmoCfg {
    pub on: bool,
    /// Чувствительность 1..10: больше — срабатывает легче.
    pub sens: u8,
    /// Сколько секунд держать эмоцию после срабатывания.
    pub seconds: f32,
}

impl Default for EmoCfg {
    fn default() -> Self {
        EmoCfg {
            on: true,
            sens: 5,
            seconds: 3.0,
        }
    }
}

/// Когда обнулять счётчик мата.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ResetMode {
    /// Только вручную.
    #[default]
    Manual,
    /// Каждые сутки (начинаются в `day_start_hour`).
    Daily,
    /// При старте трансляции в OBS.
    Stream,
}

/// Счётчик мата для OBS (то, что раньше делала отдельная программа «Солонка»).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct SwearCfg {
    pub on: bool,
    pub reset_mode: ResetMode,
    pub day_start_hour: u8,
    /// Файл для OBS (источник «Текст», «Читать из файла»).
    pub output_file: PathBuf,
    /// Текст в файле, `{count}` — число.
    pub template: String,
    /// Свои слова для счёта; `слово*` — по началу слова.
    pub extra_words: Vec<String>,
    /// Слова, которые не считать никогда.
    pub ignore_words: Vec<String>,
}

impl Default for SwearCfg {
    fn default() -> Self {
        SwearCfg {
            on: false,
            reset_mode: ResetMode::Manual,
            day_start_hour: 5,
            output_file: app_dir().join("count.txt"),
            template: "{count}".into(),
            extra_words: Vec::new(),
            ignore_words: Vec::new(),
        }
    }
}

impl SwearCfg {
    /// Настройки и счёт из Солонки (`%APPDATA%\Solonka`), если она стояла. Файл для OBS
    /// остаётся прежним — в OBS ничего менять не нужно.
    fn from_solonka() -> Option<(SwearCfg, Option<crate::counter::State>)> {
        let dir = app_dir().parent()?.join("Solonka");
        let text = std::fs::read_to_string(dir.join("config.json")).ok()?;
        let v: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
        let list = |k: &str| -> Vec<String> {
            v[k].as_array()
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default()
        };
        let cfg = SwearCfg {
            on: true,
            reset_mode: serde_json::from_value(v["reset_mode"].clone()).unwrap_or_default(),
            day_start_hour: v["day_start_hour"].as_u64().map_or(5, |h| h.min(23) as u8),
            output_file: v["output_file"]
                .as_str()
                .map(PathBuf::from)
                .unwrap_or_else(|| dir.join("count.txt")),
            template: v["template"].as_str().unwrap_or("{count}").to_string(),
            extra_words: list("extra_words"),
            ignore_words: list("ignore_words"),
        };
        let state = std::fs::read_to_string(dir.join("state.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());
        Some((cfg, state))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Имя устройства ввода; `null` — устройство по умолчанию.
    pub device: Option<String>,
    pub listening: bool,
    /// Порт локального сервера: аватар для OBS и страница настроек.
    pub port: u16,
    /// Усиление микрофона, дБ (0…40): для тихих звуковых карт.
    pub mic_gain_db: f32,
    /// RMS дБ: громче — кошка говорит.
    pub talk_db: f32,
    /// Сглаженный RMS дБ: громче (дольше ~0,15 с) — кричит.
    pub shout_db: f32,
    pub bounce: f32,
    pub breathing: bool,
    pub blink: bool,
    pub emotions: BTreeMap<String, EmoCfg>,
    /// Слова эмоций по видам (`surprise`, `sad`, `laugh`); `*` в конце — любое окончание.
    pub words: BTreeMap<String, Vec<String>>,
    /// Чувствительность распознавания крика нейросетью (когда крик записан в обучении).
    pub shout_sens: u8,
    /// Источник OBS с виджетом донатов ("" — выключено): его звук = кошка ест.
    pub donation_source: String,
    pub donation_db: f32,
    pub eat_seconds: f32,
    /// Пароль obs-websocket; пусто — берётся из настроек OBS.
    pub obs_password: String,
    pub obs_port: u16,
    /// Глобальные клавиши Ctrl+Alt+1…5, Ctrl+Alt+0.
    pub hotkeys: bool,
    pub eco_mode: bool,
    /// Ставить найденное при запуске обновление само.
    pub auto_update: bool,
    pub swears: SwearCfg,
    /// В файле ещё нет раздела `swears` (обновление со старой версии).
    #[serde(skip)]
    pub swears_new: bool,
}

impl Default for Config {
    fn default() -> Self {
        let mut emotions = BTreeMap::new();
        for e in EMOTIONS {
            let mut c = EmoCfg::default();
            match e {
                "sad" => c.seconds = 5.0,
                "surprise" => {
                    c.seconds = 2.0;
                    c.sens = 4
                }
                "sing" => c.on = false,
                _ => {}
            }
            emotions.insert(e.to_string(), c);
        }
        Config {
            device: None,
            listening: true,
            port: 7710,
            mic_gain_db: 0.0,
            talk_db: -45.0,
            shout_db: -14.0,
            bounce: 16.0,
            breathing: true,
            blink: true,
            emotions,
            words: crate::words::default_words(),
            shout_sens: 5,
            donation_source: String::new(),
            donation_db: -50.0,
            eat_seconds: 7.0,
            obs_password: String::new(),
            obs_port: 0,
            hotkeys: true,
            eco_mode: true,
            auto_update: true,
            swears: SwearCfg::default(),
            swears_new: false,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        app_dir().join("config.json")
    }

    /// `(config, ошибка, это_первый_запуск)`.
    pub fn load() -> (Config, Option<String>, bool) {
        let (mut c, err, first) = Self::load_file();
        // Первый запуск со счётчиком мата: переносим настройки и счёт из Солонки.
        if err.is_none() && c.swears_new {
            c.swears_new = false;
            if let Some((sw, state)) = SwearCfg::from_solonka() {
                c.swears = sw;
                if let Some(st) = state
                    && !crate::counter::State::path().exists()
                {
                    let _ = crate::counter::write_atomic(
                        &crate::counter::State::path(),
                        serde_json::to_string(&st).unwrap_or_default().as_bytes(),
                    );
                }
            }
            if !first {
                c.save();
            }
        }
        (c, err, first)
    }

    fn load_file() -> (Config, Option<String>, bool) {
        let path = Self::path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            let c = Config {
                swears_new: true,
                ..Config::default()
            };
            return (c, None, true);
        };
        match serde_json::from_str::<Config>(&text) {
            Ok(mut c) => {
                c.swears_new = !text.contains("\"swears\"");
                // новые эмоции, которых не было в старом файле
                let d = Config::default();
                for (k, v) in d.emotions {
                    c.emotions.entry(k).or_insert(v);
                }
                for (k, v) in d.words {
                    c.words.entry(k).or_insert(v);
                }
                (c, None, false)
            }
            Err(e) => (
                Config::default(),
                Some(format!("Ошибка в {}: {e}", path.display())),
                false,
            ),
        }
    }

    pub fn save(&self) {
        let path = Self::path();
        let _ = std::fs::create_dir_all(app_dir());
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }

    pub fn emo(&self, e: &str) -> EmoCfg {
        self.emotions.get(e).cloned().unwrap_or_default()
    }

    /// Папка с моделями рядом с exe (`SOUND_MODEL`, `WORDS_MODEL`).
    pub fn model_dir() -> PathBuf {
        exe_dir().join("models")
    }
}

/// Папка программы (рядом с exe).
pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_default()
}

pub fn app_dir() -> PathBuf {
    appdata().join("Vox")
}

fn appdata() -> PathBuf {
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default()
}

/// Программа раньше называлась «Мурлыка»: её настройки, записи голоса и счёт переезжают
/// в папку Vox при первом запуске.
pub fn migrate_old_dir() {
    let (old, new) = (appdata().join("Murlyka"), app_dir());
    if new.exists() || !old.is_dir() {
        return;
    }
    if std::fs::rename(&old, &new).is_err() && copy_dir(&old, &new).is_ok() {
        let _ = std::fs::remove_dir_all(&old);
    }
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let dst = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &dst)?;
        } else {
            std::fs::copy(e.path(), dst)?;
        }
    }
    Ok(())
}

/// Файл счётчика, который можно принять со страницы: обычный `.txt` на локальном диске
/// (`C:\…`), без сетевых путей, `\?\`, `..` и потоков NTFS (`файл:поток`).
pub fn safe_output_file(p: &std::path::Path) -> bool {
    let s = p.to_string_lossy();
    let b = s.as_bytes();
    b.len() > 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b[2] == b'\\' || b[2] == b'/')
        && !s[2..].contains(':')
        && !p.components().any(|c| matches!(c, std::path::Component::ParentDir))
        && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("txt"))
}

#[cfg(test)]
mod output_file_tests {
    use super::safe_output_file as ok;
    use std::path::Path;

    #[test]
    fn output_file_only_local_txt() {
        assert!(ok(Path::new(r"C:\Users\u\AppData\Roaming\Vox\count.txt")));
        assert!(ok(Path::new(r"D:\obs\МАТЫ.TXT")));
        assert!(!ok(Path::new(r"C:\x\run.bat")));
        assert!(!ok(Path::new(r"\host\share\count.txt")));
        assert!(!ok(Path::new(r"\?\C:\x\count.txt")));
        assert!(!ok(Path::new(r"C:\x\a.bat:s.txt")));
        assert!(!ok(Path::new(r"C:\x\..\y\count.txt")));
        assert!(!ok(Path::new("count.txt")));
    }
}
