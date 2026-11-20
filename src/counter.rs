//! Счётчик мата (бывшая отдельная программа «Солонка»): значение, автосброс и файл для OBS.
//! Мат ищется в тех же распознанных фразах, что и слова эмоций, — второе распознавание
//! речи не нужно.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{ResetMode, SwearCfg, app_dir};
use crate::win;

/// Подложка для OBS: подпись «маты» нарисована на ней, в файл пишется только число.
const OVERLAY: &[u8] = include_bytes!("../assets/overlay.png");
/// Подложка из Солонки и Мурлыки (с солонкой): её меняем на новую, свою картинку не трогаем.
const OLD_OVERLAY_SHA256: &str = "112668d97242ce0fdb1bdbee3857fe51ce32a26f3de7f26de8fcdb87d107f7a0";

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct State {
    pub count: u64,
    /// Номер суток последнего сброса или проверки (режим «каждый день»).
    pub day: i64,
}

impl State {
    pub fn path() -> PathBuf {
        app_dir().join("swears.json")
    }

    pub fn load() -> State {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        if let Ok(s) = serde_json::to_string(self) {
            let _ = write_atomic(&Self::path(), s.as_bytes());
        }
    }
}

pub struct Counter {
    state: State,
    cfg: SwearCfg,
}

impl Counter {
    pub fn new(cfg: &SwearCfg) -> Counter {
        let mut c = Counter {
            state: State::load(),
            cfg: cfg.clone(),
        };
        if c.cfg.on {
            c.roll_day();
            c.refresh_overlay();
            c.write_output();
        }
        c
    }

    /// Старая подложка рядом с файлом для OBS → новая.
    fn refresh_overlay(&self) {
        use sha2::{Digest, Sha256};
        let Some(path) = self.cfg.output_file.parent().map(|d| d.join("overlay.png")) else {
            return;
        };
        let is_old = std::fs::read(&path).is_ok_and(|b| {
            Sha256::digest(&b).iter().map(|x| format!("{x:02x}")).collect::<String>() == OLD_OVERLAY_SHA256
        });
        if is_old {
            let _ = write_atomic(&path, OVERLAY);
        }
    }

    pub fn count(&self) -> u64 {
        self.state.count
    }

    pub fn file(&self) -> &Path {
        &self.cfg.output_file
    }

    pub fn apply_config(&mut self, cfg: &SwearCfg) {
        if *cfg == self.cfg {
            return;
        }
        // При переключении в режим «каждый день» отсчёт суток начинается сейчас, без сброса.
        if cfg.reset_mode == ResetMode::Daily && self.cfg.reset_mode != ResetMode::Daily {
            self.state.day = win::local_day_key(cfg.day_start_hour);
            self.state.save();
        }
        self.cfg = cfg.clone();
        if self.cfg.on {
            self.refresh_overlay();
            self.write_output();
        }
    }

    pub fn add(&mut self, n: u32) {
        if n == 0 {
            return;
        }
        self.roll_day();
        self.state.count += u64::from(n);
        self.persist();
    }

    pub fn reset(&mut self) {
        self.state.count = 0;
        self.state.day = win::local_day_key(self.cfg.day_start_hour);
        self.persist();
    }

    /// Старт трансляции в OBS: сброс в режиме «каждый стрим».
    pub fn on_stream_started(&mut self) -> bool {
        let yes = self.cfg.reset_mode == ResetMode::Stream;
        if yes {
            self.reset();
        }
        yes
    }

    /// Режим «каждый день»: наступили новые сутки — сброс. `true`, если сбросили.
    pub fn roll_day(&mut self) -> bool {
        if self.cfg.reset_mode != ResetMode::Daily {
            return false;
        }
        let today = win::local_day_key(self.cfg.day_start_hour);
        if self.state.day == today {
            return false;
        }
        self.state.day = today;
        self.state.count = 0;
        self.persist();
        true
    }

    fn persist(&self) {
        self.state.save();
        if self.cfg.on {
            self.write_output();
        }
    }

    /// Число в файл для OBS; рядом — подложка, если её там нет (например, папку удалили).
    fn write_output(&self) {
        let text = self.cfg.template.replace("{count}", &self.state.count.to_string());
        let _ = write_atomic(&self.cfg.output_file, text.as_bytes());
        if let Some(dir) = self.cfg.output_file.parent() {
            let overlay = dir.join("overlay.png");
            if !overlay.exists() {
                let _ = std::fs::write(overlay, OVERLAY);
            }
        }
    }
}

/// Запись через временный файл: OBS никогда не увидит половину числа. OBS может держать
/// файл открытым — тогда пробуем ещё пару раз.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    for _ in 0..3 {
        if std::fs::rename(&tmp, path).is_ok() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    let _ = std::fs::remove_file(&tmp);
    Err(std::io::Error::other("файл занят"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_template_and_overlay() {
        let dir = std::env::temp_dir().join(format!("vox-counter-{}", std::process::id()));
        let cfg = SwearCfg {
            on: true,
            output_file: dir.join("count.txt"),
            template: "Маты: {count}".into(),
            ..SwearCfg::default()
        };
        let mut c = Counter {
            state: State::default(),
            cfg,
        };
        c.write_output();
        c.state.count = 7;
        c.write_output();
        assert_eq!(std::fs::read_to_string(dir.join("count.txt")).unwrap(), "Маты: 7");
        assert!(dir.join("overlay.png").exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
