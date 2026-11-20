#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Vox — голосовой помощник стримера. Трей + микрофон → громкость, распознавание звуков и слов →
//! состояние аватара (страница, которую показывает OBS), эмоции, донаты и счётчик мата.

mod asr;
mod audio;
mod config;
mod counter;
mod engine;
mod hub;
mod icon;
mod matcher;
mod obs;
mod phrase;
mod prosody;
mod server;
mod tagger;
mod update;
mod voice;
mod win;
mod words;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tray_icon::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, TranslateMessage, WM_HOTKEY,
};

use config::{Config, ResetMode};
use engine::{Ctrl, Engine, ModelState, SharedModel};
use hub::{Cmd, Hub, SharedHub};
use obs::ObsClient;

/// Ctrl+Alt+<клавиша> → эмоция.
const HOTKEYS: [(i32, char, &str); 6] = [
    (1, '1', "laugh"),
    (2, '2', "sad"),
    (3, '3', "surprise"),
    (4, '4', "sing"),
    (5, '5', "eat"),
    (6, '0', "none"),
];

/// Подписи эмоций в меню.
const EMO_TITLES: [(&str, &str); 4] = [
    ("laugh", "Смех"),
    ("sad", "Грусть"),
    ("surprise", "Удивление"),
    ("sing", "Пение"),
];
/// Показать вручную: эмоции, еда и сброс.
const SHOW_TITLES: [(&str, &str); 6] = [
    ("laugh", "Смех"),
    ("sad", "Грусть"),
    ("surprise", "Удивление"),
    ("sing", "Пение"),
    ("eat", "Еда (как на донат)"),
    ("none", "Сбросить"),
];
const RESET_TITLES: [(ResetMode, &str); 3] = [
    (ResetMode::Manual, "Только вручную"),
    (ResetMode::Daily, "Каждый день"),
    (ResetMode::Stream, "При старте стрима в OBS"),
];
const AUTHOR_URL: &str = "https://www.twitch.tv/banshee_nt";

/// Список, который меняется на ходу (микрофоны, источники OBS): галочки + значение пункта.
struct Choice {
    sub: Submenu,
    items: Vec<(CheckMenuItem, Option<String>)>,
    /// Что сейчас показано — чтобы не пересобирать подменю без нужды.
    shown: Vec<Option<String>>,
}

impl Choice {
    fn new(title: &str) -> Choice {
        Choice {
            sub: Submenu::new(title, true),
            items: Vec::new(),
            shown: Vec::new(),
        }
    }

    /// Пересобрать пункты; `head` — пункты перед списком, `tail` — после.
    fn fill(&mut self, values: Vec<(String, Option<String>)>, tail: &[&dyn IsMenuItem]) {
        let keys: Vec<Option<String>> = values.iter().map(|(_, v)| v.clone()).collect();
        if keys == self.shown && !self.items.is_empty() {
            return;
        }
        for (item, _) in self.items.drain(..) {
            let _ = self.sub.remove(&item);
        }
        for t in tail {
            let _ = self.sub.remove(*t);
        }
        for (title, value) in values {
            let item = CheckMenuItem::new(shorten(&title, 60), true, false, None);
            let _ = self.sub.append(&item);
            self.items.push((item, value));
        }
        for t in tail {
            let _ = self.sub.append(*t);
        }
        self.shown = keys;
    }

    fn check(&self, selected: Option<&str>) {
        for (item, v) in &self.items {
            item.set_checked(v.as_deref() == selected);
        }
    }

    fn find(&self, id: &MenuId) -> Option<Option<String>> {
        self.items.iter().find(|(i, _)| i.id() == id).map(|(_, v)| v.clone())
    }
}

struct Menus {
    menu: Menu,
    status: MenuItem,
    obs: MenuItem,
    swears: MenuItem,
    settings: MenuItem,
    copy_url: MenuItem,
    listen: CheckMenuItem,
    mic: Choice,
    mic_sep: PredefinedMenuItem,
    mic_refresh: MenuItem,
    emo_on: Vec<(CheckMenuItem, &'static str)>,
    emo_show: Vec<(MenuItem, &'static str)>,
    sw_on: CheckMenuItem,
    sw_reset: MenuItem,
    sw_modes: Vec<(CheckMenuItem, ResetMode)>,
    sw_folder: MenuItem,
    sw_copy: MenuItem,
    donation: Choice,
    donation_hint: MenuItem,
    autostart: CheckMenuItem,
    update: MenuItem,
    author: MenuItem,
    quit: MenuItem,
}

struct App {
    cfg: Config,
    hub: SharedHub,
    model: SharedModel,
    engine: Engine,
    obs: ObsClient,
    capture: Option<audio::Capture>,
    capture_note: Option<String>,
    capture_suspended: bool,
    last_auto_restart: Option<Instant>,
    server_error: Option<String>,
    hotkeys_on: bool,
    busy_hotkeys: Vec<char>,
    updater: update::Updater,
    tray: TrayIcon,
    menus: Menus,
    shown_icon: Option<icon::Kind>,
    shown_status: String,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 2 && args[1] == "--eval" {
        if let Err(e) = engine::run_eval() {
            eprintln!("ошибка: {e}");
        }
        return;
    }
    if args.len() == 3 && args[1] == "--swears" {
        if let Err(e) = engine::run_swears(&args[2]) {
            eprintln!("ошибка: {e}");
        }
        return;
    }
    if args.len() == 3 && args[1] == "--wav" {
        if let Err(e) = engine::run_wav(&args[2]) {
            eprintln!("ошибка: {e}");
        }
        return;
    }
    config::migrate_old_dir();
    let (cfg, cfg_error, first_run) = Config::load();
    if !win::acquire_single_instance() {
        // Уже запущена — просто открываем настройки.
        win::shell_open(settings_url(&cfg));
        return;
    }
    win::register_main_thread();
    win::migrate_autostart();
    if let Some(e) = cfg_error {
        win::error_box(&format!("{e}\n\nИспользуются настройки по умолчанию."));
    }
    win::make_process_humble(cfg.eco_mode);

    let hub: SharedHub = Arc::new(Hub::default());
    let model: SharedModel = Arc::new(Mutex::new(ModelState::Loading));
    let engine = Engine::spawn(Config::model_dir(), cfg.clone(), hub.clone(), model.clone());
    let obs = ObsClient::spawn(&cfg, hub.clone(), engine.sender());
    let server_error = server::spawn(cfg.port, hub.clone()).err();
    let updater = update::Updater::spawn(cfg.auto_update, hub.clone());

    TrayIconEvent::set_event_handler(Some(|e| {
        if matches!(e, TrayIconEvent::DoubleClick { .. }) {
            win::notify_main(win::EV_OPEN_SETTINGS);
        }
    }));
    let menus = build_menu(&cfg);
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menus.menu.clone()))
        .with_tooltip(win::APP_NAME)
        .with_icon(icon::make(icon::Kind::Busy))
        .build()
        .expect("tray icon");

    let mut app = App {
        cfg,
        hub,
        model,
        engine,
        obs,
        capture: None,
        capture_note: None,
        capture_suspended: false,
        last_auto_restart: None,
        server_error,
        hotkeys_on: false,
        busy_hotkeys: Vec::new(),
        updater,
        tray,
        menus,
        shown_icon: Some(icon::Kind::Busy),
        shown_status: String::new(),
    };
    app.sync_hotkeys();
    app.publish_cfg();
    app.publish_mics();
    app.update();
    if let Some(e) = &app.server_error {
        win::error_box(e);
    } else if first_run {
        app.cfg.save();
        win::shell_open(settings_url(&app.cfg));
    }

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    loop {
        if unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } <= 0 {
            break;
        }
        if msg.message == WM_HOTKEY {
            if let Some((.., e)) = HOTKEYS.iter().find(|(id, ..)| *id as usize == msg.wParam) {
                app.engine.send(Ctrl::Manual(e.to_string()));
            }
            continue;
        }
        if !msg.hwnd.is_null() {
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let mut changed = false;
        let pending = win::take_pending();
        if pending & win::EV_STREAM_ERROR != 0 {
            app.on_stream_error();
            changed = true;
        }
        if pending & win::EV_CMD != 0 {
            for c in app.hub.take_cmds() {
                app.on_cmd(c);
            }
            changed = true;
        }
        if pending & win::EV_OPEN_SETTINGS != 0 {
            win::shell_open(settings_url(&app.cfg));
        }
        if pending & win::EV_QUIT != 0 {
            return;
        }
        changed |= pending & win::EV_REFRESH != 0;
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == *app.menus.quit.id() {
                return;
            }
            app.on_menu(&ev);
            changed = true;
        }
        if changed {
            app.update();
        }
    }
}

fn avatar_url(cfg: &Config) -> String {
    format!("http://127.0.0.1:{}/", cfg.port)
}

fn settings_url(cfg: &Config) -> String {
    format!("http://127.0.0.1:{}/settings", cfg.port)
}

fn build_menu(cfg: &Config) -> Menus {
    let mut m = Menus {
        menu: Menu::new(),
        status: MenuItem::new("Запуск…", false, None),
        obs: MenuItem::new("OBS: подключаюсь…", false, None),
        swears: MenuItem::new("", false, None),
        settings: MenuItem::new("Открыть настройки", true, None),
        copy_url: MenuItem::new("Скопировать ссылку на аватар для OBS", true, None),
        listen: CheckMenuItem::new("Слушать микрофон", true, cfg.listening, None),
        mic: Choice::new("Микрофон"),
        mic_sep: PredefinedMenuItem::separator(),
        mic_refresh: MenuItem::new("Обновить список", true, None),
        emo_on: EMO_TITLES
            .iter()
            .map(|&(e, t)| (CheckMenuItem::new(t, true, cfg.emo(e).on, None), e))
            .collect(),
        emo_show: SHOW_TITLES
            .iter()
            .map(|&(e, t)| (MenuItem::new(t, true, None), e))
            .collect(),
        sw_on: CheckMenuItem::new("Считать мат", true, cfg.swears.on, None),
        sw_reset: MenuItem::new("Обнулить сейчас", true, None),
        sw_modes: RESET_TITLES
            .iter()
            .map(|&(mode, t)| (CheckMenuItem::new(t, true, cfg.swears.reset_mode == mode, None), mode))
            .collect(),
        sw_folder: MenuItem::new("Открыть папку с файлом для OBS", true, None),
        sw_copy: MenuItem::new("Скопировать путь к файлу", true, None),
        donation: Choice::new("Донат-алерты"),
        donation_hint: MenuItem::new("Список источников — когда OBS подключён", false, None),
        autostart: CheckMenuItem::new(
            "Запускать вместе с Windows",
            true,
            win::autostart_enabled(),
            None,
        ),
        update: MenuItem::new(format!("Проверить обновления (v{})", update::VERSION), true, None),
        author: MenuItem::new("Автор: banshee — стримы на Twitch", true, None),
        quit: MenuItem::new("Выход", true, None),
    };
    let sep = PredefinedMenuItem::separator;

    let emotions = Submenu::new("Эмоции", true);
    let _ = emotions.append(&MenuItem::new("Узнавать сами:", false, None));
    for (i, _) in &m.emo_on {
        let _ = emotions.append(i);
    }
    let show = Submenu::new("Показать сейчас", true);
    for (i, _) in &m.emo_show {
        let _ = show.append(i);
    }
    let _ = emotions.append_items(&[&sep(), &show]);

    let swears = Submenu::new("Счётчик мата", true);
    let modes = Submenu::new("Обнулять автоматически", true);
    for (i, _) in &m.sw_modes {
        let _ = modes.append(i);
    }
    let _ = swears.append_items(&[
        &m.sw_on,
        &m.sw_reset,
        &modes,
        &sep(),
        &m.sw_folder,
        &m.sw_copy,
    ]);

    fill_mics(&mut m, cfg);
    fill_donation(&mut m, cfg, &[]);

    let _ = m.menu.append_items(&[
        &m.status,
        &m.obs,
        &m.swears,
        &sep(),
        &m.settings,
        &m.copy_url,
        &sep(),
        &m.listen,
        &m.mic.sub,
        &emotions,
        &swears,
        &m.donation.sub,
        &sep(),
        &m.autostart,
        &m.update,
        &m.author,
        &m.quit,
    ]);
    m
}

fn fill_mics(m: &mut Menus, cfg: &Config) {
    let mut values = vec![("По умолчанию (Windows)".to_string(), None)];
    values.extend(audio::input_device_names().into_iter().map(|n| (n.clone(), Some(n))));
    m.mic.shown.clear(); // «Обновить список» — всегда пересобираем
    m.mic.fill(values, &[&m.mic_sep, &m.mic_refresh]);
    m.mic.check(cfg.device.as_deref());
}

fn fill_donation(m: &mut Menus, cfg: &Config, inputs: &[String]) {
    let mut values = vec![("Выключено".to_string(), Some(String::new()))];
    let mut list: Vec<String> = inputs.to_vec();
    if !cfg.donation_source.is_empty() && !list.contains(&cfg.donation_source) {
        list.insert(0, cfg.donation_source.clone());
    }
    values.extend(list.into_iter().map(|n| (n.clone(), Some(n))));
    let keys: Vec<Option<String>> = values.iter().map(|(_, v)| v.clone()).collect();
    if keys != m.donation.shown || m.donation.items.is_empty() {
        // подсказка «когда OBS подключён» — только при пустом списке источников
        let _ = m.donation.sub.remove(&m.donation_hint);
        let hint: &[&dyn IsMenuItem] = if inputs.is_empty() { &[&m.donation_hint] } else { &[] };
        m.donation.fill(values, hint);
    }
    m.donation.check(Some(cfg.donation_source.as_str()));
}

impl App {
    fn on_menu(&mut self, ev: &MenuEvent) {
        let id = &ev.id;
        let m = &self.menus;
        if id == m.settings.id() {
            win::shell_open(settings_url(&self.cfg));
        } else if id == m.copy_url.id() {
            if win::copy_text(&avatar_url(&self.cfg)) {
                win::info_box(&format!(
                    "Ссылка скопирована: {}\n\nВ OBS: Источники → + → Браузер → вставь её в поле URL \
                     (галочку «Локальный файл» не ставь). Ширина и высота 600.",
                    avatar_url(&self.cfg)
                ));
            }
        } else if id == m.listen.id() {
            self.apply_patch(json!({ "listening": !self.cfg.listening }));
        } else if id == m.mic_refresh.id() {
            let cfg = self.cfg.clone();
            fill_mics(&mut self.menus, &cfg);
            self.capture_suspended = false;
            self.publish_mics();
        } else if let Some(dev) = m.mic.find(id) {
            self.apply_patch(json!({ "device": dev }));
        } else if let Some((_, e)) = m.emo_on.iter().find(|(i, _)| i.id() == id) {
            let on = !self.cfg.emo(e).on;
            self.apply_patch(json!({ "emotions": { *e: { "on": on } } }));
        } else if let Some((_, e)) = m.emo_show.iter().find(|(i, _)| i.id() == id) {
            self.engine.send(Ctrl::Manual(e.to_string()));
        } else if id == m.sw_on.id() {
            self.apply_patch(json!({ "swears": { "on": !self.cfg.swears.on } }));
        } else if id == m.sw_reset.id() {
            self.engine.send(Ctrl::SwearReset);
        } else if let Some((_, mode)) = m.sw_modes.iter().find(|(i, _)| i.id() == id) {
            self.apply_patch(json!({ "swears": { "reset_mode": mode } }));
        } else if id == m.sw_folder.id() {
            if let Some(dir) = self.cfg.swears.output_file.parent() {
                let _ = std::fs::create_dir_all(dir);
                win::shell_open(dir);
            }
        } else if id == m.sw_copy.id() {
            win::copy_text(&self.cfg.swears.output_file.display().to_string());
        } else if let Some(src) = m.donation.find(id) {
            self.apply_patch(json!({ "donation_source": src.unwrap_or_default() }));
        } else if id == m.autostart.id() {
            let on = !win::autostart_enabled();
            if !win::set_autostart(on) {
                win::error_box("Не удалось изменить автозапуск.");
            }
            self.publish_cfg();
        } else if id == m.update.id() {
            if update::found().is_some() {
                self.updater.install_found();
            } else {
                self.updater.check_now();
            }
        } else if id == m.author.id() {
            win::shell_open(AUTHOR_URL);
        }
    }

    fn on_cmd(&mut self, c: Cmd) {
        match c {
            Cmd::Set(v) => self.apply_patch(v),
            Cmd::Emotion(e) => self.engine.send(Ctrl::Manual(e)),
            Cmd::Train(e) => self.engine.send(Ctrl::Train(e)),
            Cmd::Untrain(e) => self.engine.send(Ctrl::Untrain(e)),
            Cmd::RefreshMics => self.publish_mics(),
            Cmd::SwearReset => self.engine.send(Ctrl::SwearReset),
            Cmd::UpdateCheck => self.updater.check_now(),
            Cmd::UpdateInstall => self.updater.install_found(),
        }
    }

    /// Частичные настройки со страницы: накладываем на текущие, проверяем и применяем.
    fn apply_patch(&mut self, patch: Value) {
        let mut cur = serde_json::to_value(&self.cfg).unwrap_or_default();
        if let Some(autostart) = patch.get("autostart").and_then(Value::as_bool) {
            if autostart != win::autostart_enabled() {
                win::set_autostart(autostart);
            }
        }
        merge(&mut cur, &patch);
        let Ok(mut new) = serde_json::from_value::<Config>(cur) else {
            return;
        };
        // Счётчик пишет только .txt на локальном диске: не exe/bat, не сетевой путь.
        if !config::safe_output_file(&new.swears.output_file) {
            new.swears.output_file = self.cfg.swears.output_file.clone();
        }
        let device_changed = new.device != self.cfg.device;
        let listening_changed = new.listening != self.cfg.listening;
        self.cfg = new;
        self.cfg.save();
        self.engine.send(Ctrl::Config(Box::new(self.cfg.clone())));
        self.obs.apply_config(&self.cfg);
        self.updater.set_auto(self.cfg.auto_update);
        if device_changed {
            self.restart_capture();
            self.publish_mics();
        }
        if listening_changed {
            self.capture_suspended = false;
        }
        self.sync_hotkeys();
        self.publish_cfg();
    }

    fn sync_hotkeys(&mut self) {
        let keys: Vec<(i32, char)> = HOTKEYS.iter().map(|(id, ch, _)| (*id, *ch)).collect();
        if self.cfg.hotkeys && !self.hotkeys_on {
            self.busy_hotkeys = win::register_hotkeys(&keys);
            self.hotkeys_on = true;
        } else if !self.cfg.hotkeys && self.hotkeys_on {
            win::unregister_hotkeys(&keys);
            self.busy_hotkeys.clear();
            self.hotkeys_on = false;
        }
    }

    fn publish_cfg(&self) {
        let mut cfg = serde_json::to_value(&self.cfg).unwrap_or_default();
        cfg["autostart"] = win::autostart_enabled().into();
        // Пароль OBS странице не нужен: не отдаём его наружу.
        if let Some(o) = cfg.as_object_mut() {
            o.remove("obs_password");
        }
        let msg = json!({
            "t": "cfg",
            "cfg": cfg,
            "url": avatar_url(&self.cfg),
            "busyHotkeys": self.busy_hotkeys.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
        });
        self.hub.sticky("cfg", msg.to_string());
    }

    fn publish_mics(&self) {
        let current = self.capture.as_ref().map(|c| c.device_name.clone());
        let msg = json!({
            "t": "mics",
            "list": audio::input_device_names(),
            "selected": self.cfg.device,
            "current": current,
        });
        self.hub.sticky("mics", msg.to_string());
    }

    fn on_stream_error(&mut self) {
        self.stop_capture();
        let recently = self
            .last_auto_restart
            .is_some_and(|t| t.elapsed() < Duration::from_secs(10));
        if recently {
            self.capture_suspended = true;
            self.capture_note = Some("Микрофон пропал — выберите его в настройках".into());
        } else {
            self.last_auto_restart = Some(Instant::now());
        }
    }

    fn stop_capture(&mut self) {
        if self.capture.take().is_some() {
            self.engine.send(Ctrl::Stop);
        }
    }

    fn restart_capture(&mut self) {
        self.stop_capture();
        self.capture_suspended = false;
        self.capture_note = None;
        self.sync_capture();
    }

    fn sync_capture(&mut self) {
        let want = self.cfg.listening && !self.capture_suspended;
        if want && self.capture.is_none() {
            match audio::start(self.cfg.device.as_deref(), &self.engine) {
                Ok((cap, note)) => {
                    self.capture = Some(cap);
                    self.capture_note = note;
                    self.publish_mics();
                }
                Err(e) => {
                    self.capture_suspended = true;
                    self.capture_note = Some(e);
                }
            }
        } else if !want {
            self.stop_capture();
        }
    }

    fn update(&mut self) {
        self.sync_capture();
        let model = self.model.lock().unwrap().clone();
        self.sync_menu();
        let m = &self.menus;

        let (text, kind) = match (&self.server_error, &model, &self.capture, &self.capture_note) {
            (Some(e), ..) => (e.clone(), icon::Kind::Error),
            (_, ModelState::Error(e), ..) => (e.clone(), icon::Kind::Error),
            _ if !self.cfg.listening => ("На паузе".to_string(), icon::Kind::Paused),
            (_, ModelState::Loading, ..) => ("Загрузка…".to_string(), icon::Kind::Busy),
            (_, _, Some(cap), note) => (
                note.clone()
                    .unwrap_or_else(|| format!("Слушаю: {}", cap.device_name)),
                icon::Kind::Listening,
            ),
            (_, _, None, note) => (
                note.clone().unwrap_or_else(|| "Микрофон не запущен".into()),
                icon::Kind::Error,
            ),
        };
        m.status.set_text(shorten(&text, 60));
        if self.shown_icon != Some(kind) {
            let _ = self.tray.set_icon(Some(icon::make(kind)));
            self.shown_icon = Some(kind);
        }
        if text != self.shown_status {
            let _ = self
                .tray
                .set_tooltip(Some(format!("{} — {}", win::APP_NAME, shorten(&text, 90))));
            let ok = kind == icon::Kind::Listening;
            self.hub.sticky(
                "status",
                json!({ "t": "status", "text": text, "ok": ok }).to_string(),
            );
            self.shown_status = text;
        }
    }
}

impl App {
    /// Галочки и строки состояния в меню трея — по настройкам и последним данным.
    fn sync_menu(&mut self) {
        let obs = self.hub.last("obs");
        let inputs: Vec<String> = obs
            .as_ref()
            .and_then(|v| v["inputs"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        let cfg = self.cfg.clone();
        fill_donation(&mut self.menus, &cfg, &inputs);
        let m = &self.menus;
        m.listen.set_checked(cfg.listening);
        m.autostart.set_checked(win::autostart_enabled());
        m.update.set_text(match update::found() {
            Some(v) => format!("Установить обновление {v}"),
            None => format!("Проверить обновления (v{})", update::VERSION),
        });
        m.mic.check(cfg.device.as_deref());
        for (i, e) in &m.emo_on {
            i.set_checked(cfg.emo(e).on);
        }
        m.sw_on.set_checked(cfg.swears.on);
        for (i, mode) in &m.sw_modes {
            i.set_checked(cfg.swears.reset_mode == *mode);
        }
        let obs_text = match obs.as_ref().and_then(|v| v["status"].as_str().map(String::from)) {
            Some(s) if s == "ok" => "OBS: подключено".to_string(),
            Some(s) => format!("OBS: {}", s.trim_end_matches('…').to_lowercase()),
            None => "OBS: подключаюсь…".into(),
        };
        m.obs.set_text(shorten(&obs_text, 60));
        let count = self.hub.last("swears").and_then(|v| v["count"].as_u64()).unwrap_or(0);
        m.swears.set_text(if cfg.swears.on {
            format!("Маты: {count}")
        } else {
            "Счётчик мата выключен".into()
        });
    }
}

/// Рекурсивно накладывает `patch` на `base` (объекты сливаются, остальное заменяется).
fn merge(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            for (k, v) in p {
                merge(b.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (b, p) => *b = p.clone(),
    }
}

fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_merges_nested_emotions() {
        let mut base = serde_json::to_value(Config::default()).unwrap();
        merge(&mut base, &json!({ "talk_db": -30.0, "emotions": { "laugh": { "sens": 9 } } }));
        let c: Config = serde_json::from_value(base).unwrap();
        assert_eq!(c.talk_db, -30.0);
        assert_eq!(c.emo("laugh").sens, 9);
        assert!(c.emo("laugh").on);
        assert_eq!(c.emo("sad").seconds, 5.0);
    }
}
