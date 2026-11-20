//! Связь потоков с веб-страницами: список подключённых по WebSocket страниц (аватар в OBS,
//! настройки), рассылка сообщений и очередь команд от страницы настроек к главному потоку.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::win;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Avatar,
    Settings,
}

/// Команды от страницы настроек.
#[derive(Debug)]
pub enum Cmd {
    /// Частичные настройки (JSON-объект с полями `Config`).
    Set(Value),
    /// Эмоция вручную: имя или "none"; "eat" — еда.
    Emotion(String),
    /// Записать пример для обучения.
    Train(String),
    /// Удалить примеры класса.
    Untrain(String),
    RefreshMics,
    /// Обнулить счётчик мата.
    SwearReset,
    /// Проверить обновления сейчас.
    UpdateCheck,
    /// Поставить найденное обновление.
    UpdateInstall,
}

#[derive(Default)]
pub struct Hub {
    clients: Mutex<Vec<(u64, Kind, Sender<String>)>>,
    next_id: AtomicU64,
    settings_open: AtomicUsize,
    /// Последнее сообщение каждого типа — новые страницы получают их сразу при подключении.
    sticky: Mutex<BTreeMap<&'static str, String>>,
    cmds: Mutex<VecDeque<Cmd>>,
}

pub type SharedHub = Arc<Hub>;

impl Hub {
    pub fn add(&self, kind: Kind) -> (u64, Receiver<String>) {
        let (tx, rx) = mpsc::channel();
        for msg in self.sticky.lock().unwrap().values() {
            let _ = tx.send(msg.clone());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.clients.lock().unwrap().push((id, kind, tx));
        if kind == Kind::Settings {
            self.settings_open.fetch_add(1, Ordering::AcqRel);
        }
        (id, rx)
    }

    pub fn remove(&self, id: u64) {
        let mut c = self.clients.lock().unwrap();
        if let Some(pos) = c.iter().position(|(i, ..)| *i == id) {
            let (_, kind, _) = c.remove(pos);
            if kind == Kind::Settings {
                self.settings_open.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    /// Открыта ли сейчас страница настроек (тогда считаем и шлём живые полоски).
    pub fn settings_open(&self) -> bool {
        self.settings_open.load(Ordering::Acquire) > 0
    }

    /// Разослать всем (или только страницам настроек).
    pub fn send(&self, only_settings: bool, msg: &str) {
        let c = self.clients.lock().unwrap();
        for (_, kind, tx) in c.iter() {
            if !only_settings || *kind == Kind::Settings {
                let _ = tx.send(msg.to_string());
            }
        }
    }

    /// Разослать и запомнить для новых страниц. Состояние OBS и счётчик мата показывает ещё
    /// и меню в трее — при их изменении будим главный поток.
    pub fn sticky(&self, key: &'static str, msg: String) {
        self.send(false, &msg);
        let old = self.sticky.lock().unwrap().insert(key, msg.clone());
        if matches!(key, "obs" | "swears") && old.as_deref() != Some(msg.as_str()) {
            win::notify_main(win::EV_REFRESH);
        }
    }

    /// Последнее сообщение типа `key` (JSON).
    pub fn last(&self, key: &str) -> Option<Value> {
        let s = self.sticky.lock().unwrap();
        s.get(key).and_then(|m| serde_json::from_str(m).ok())
    }

    pub fn push_cmd(&self, c: Cmd) {
        self.cmds.lock().unwrap().push_back(c);
        win::notify_main(win::EV_CMD);
    }

    pub fn take_cmds(&self) -> Vec<Cmd> {
        self.cmds.lock().unwrap().drain(..).collect()
    }
}
