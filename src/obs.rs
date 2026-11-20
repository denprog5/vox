//! Клиент obs-websocket v5 (встроен в OBS 28+). Нужен для двух вещей:
//! список источников (чтобы выбрать виджет донатов) и громкость этого источника —
//! когда алерт звучит, кошка ест. Порт и пароль берутся из настроек самого OBS.

use std::io::ErrorKind;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::thread::{self, Thread};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tungstenite::{Message, WebSocket};

use crate::config::Config;
use crate::engine::Ctrl;
use crate::hub::SharedHub;

const SUB_INPUTS: u32 = 1 << 3;
/// Старт и остановка трансляции (сброс счётчика мата «при старте стрима»).
const SUB_OUTPUTS: u32 = 1 << 6;
const SUB_VOLUME_METERS: u32 = 1 << 16;
const RETRY: Duration = Duration::from_secs(5);

#[derive(Clone, PartialEq)]
struct Settings {
    donation_source: String,
    port: u16,
    password: String,
}

pub struct ObsClient {
    settings: Arc<Mutex<Settings>>,
    thread: Thread,
}

/// Как передать событие в поток анализа.
pub type EngineTx = Arc<dyn Fn(Ctrl) + Send + Sync>;

impl ObsClient {
    pub fn spawn(cfg: &Config, hub: SharedHub, to_engine: EngineTx) -> ObsClient {
        let settings = Arc::new(Mutex::new(settings_of(cfg)));
        let s = settings.clone();
        let handle = thread::Builder::new()
            .name("obs".into())
            .spawn(move || run(s, hub, to_engine))
            .expect("spawn obs");
        ObsClient {
            settings,
            thread: handle.thread().clone(),
        }
    }

    pub fn apply_config(&self, cfg: &Config) {
        let new = settings_of(cfg);
        let mut cur = self.settings.lock().unwrap();
        if *cur != new {
            *cur = new;
            self.thread.unpark();
        }
    }
}

fn settings_of(cfg: &Config) -> Settings {
    Settings {
        donation_source: cfg.donation_source.clone(),
        port: cfg.obs_port,
        password: cfg.obs_password.clone(),
    }
}

fn set_status(hub: &SharedHub, status: &str, inputs: &[String]) {
    hub.sticky("obs", json!({ "t": "obs", "status": status, "inputs": inputs }).to_string());
}

fn run(settings: Arc<Mutex<Settings>>, hub: SharedHub, to_engine: EngineTx) {
    crate::win::lower_current_thread();
    loop {
        let s = settings.lock().unwrap().clone();
        set_status(&hub, "Подключаюсь к OBS…", &[]);
        match session(&s, &settings, &hub, &to_engine) {
            Ok(()) => continue,
            Err(e) => set_status(&hub, &e, &[]),
        }
        to_engine(Ctrl::DonationDb(-100.0));
        thread::park_timeout(RETRY);
    }
}

struct LocalObs {
    enabled: bool,
    port: u16,
    password: Option<String>,
}

/// Настройки WebSocket-сервера из конфига OBS (`%APPDATA%\obs-studio\plugin_config\obs-websocket`).
fn local_obs() -> Option<LocalObs> {
    let path = std::path::PathBuf::from(std::env::var_os("APPDATA")?)
        .join("obs-studio/plugin_config/obs-websocket/config.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    Some(LocalObs {
        enabled: v["server_enabled"].as_bool().unwrap_or(false),
        port: v["server_port"]
            .as_u64()
            .and_then(|p| u16::try_from(p).ok())
            .unwrap_or(4455),
        password: v["auth_required"].as_bool().unwrap_or(false).then(|| {
            v["server_password"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        }),
    })
}

/// `Ok(())` — сессию закрыли мы из-за смены настроек; `Err` — проблема соединения.
fn session(
    s: &Settings,
    settings: &Mutex<Settings>,
    hub: &SharedHub,
    to_engine: &EngineTx,
) -> Result<(), String> {
    let local = local_obs();
    let port = if s.port != 0 {
        s.port
    } else {
        local.as_ref().map_or(4455, |l| l.port)
    };
    let password = if s.password.is_empty() {
        local
            .as_ref()
            .and_then(|l| l.password.clone())
            .unwrap_or_default()
    } else {
        s.password.clone()
    };
    let url = format!("ws://127.0.0.1:{port}");
    let mut ws = connect(port, &url).map_err(|e| match &local {
        Some(l) if !l.enabled => {
            "В OBS выключен WebSocket-сервер (Инструменты → Настройки сервера WebSocket)".into()
        }
        _ => e,
    })?;

    let hello = read_json(&mut ws)?;
    let mut subs = SUB_INPUTS | SUB_OUTPUTS;
    if !s.donation_source.is_empty() {
        subs |= SUB_VOLUME_METERS;
    }
    let mut identify = json!({ "rpcVersion": 1, "eventSubscriptions": subs });
    if let Some(auth) = hello.pointer("/d/authentication") {
        if password.is_empty() {
            return Err("OBS требует пароль (obs_password в config.json)".into());
        }
        let challenge = auth["challenge"].as_str().unwrap_or_default();
        let salt = auth["salt"].as_str().unwrap_or_default();
        identify["authentication"] = auth_string(&password, salt, challenge).into();
    }
    send_json(&mut ws, &json!({ "op": 1, "d": identify }))?;
    let mut inputs: Vec<String> = Vec::new();
    let mut last_db = -100.0f32;
    let get_inputs = json!({ "op": 6, "d": { "requestType": "GetInputList", "requestId": "inputs" } });
    loop {
        let msg = match ws.read() {
            Ok(m) => m,
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                if *settings.lock().unwrap() != *s {
                    let _ = ws.close(None);
                    return Ok(());
                }
                continue;
            }
            Err(e) => return Err(format!("Связь с OBS потеряна: {e}")),
        };
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(frame) => {
                let code = frame.map(|f| u16::from(f.code)).unwrap_or(0);
                return Err(if code == 4009 {
                    "Неверный пароль OBS".into()
                } else {
                    "OBS закрыл соединение".into()
                });
            }
            _ => continue,
        };
        let Ok(v) = serde_json::from_str::<Value>(text.as_str()) else {
            continue;
        };
        match v["op"].as_u64() {
            Some(2) => {
                set_status(hub, "ok", &inputs);
                send_json(&mut ws, &get_inputs)?;
            }
            Some(7) if v.pointer("/d/requestId") == Some(&json!("inputs")) => {
                inputs = v
                    .pointer("/d/responseData/inputs")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|i| i["inputName"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                inputs.sort_by_key(|n| n.to_lowercase());
                set_status(hub, "ok", &inputs);
            }
            Some(5) => {
                let ev = v.pointer("/d/eventType").and_then(Value::as_str).unwrap_or("");
                match ev {
                    "InputVolumeMeters" => {
                        let db = v
                            .pointer("/d/eventData/inputs")
                            .and_then(Value::as_array)
                            .and_then(|a| {
                                a.iter().find(|i| i["inputName"] == s.donation_source.as_str())
                            })
                            .map_or(-100.0, |i| meter_db(&i["inputLevelsMul"]));
                        // Тишину шлём один раз, а не 20 раз в секунду.
                        if db > -99.0 || last_db > -99.0 {
                            to_engine(Ctrl::DonationDb(db));
                        }
                        last_db = db;
                    }
                    "StreamStateChanged"
                        if v.pointer("/d/eventData/outputState")
                            == Some(&json!("OBS_WEBSOCKET_OUTPUT_STARTED")) =>
                    {
                        to_engine(Ctrl::StreamStarted);
                    }
                    "InputCreated" | "InputRemoved" | "InputNameChanged" => {
                        send_json(&mut ws, &get_inputs)?;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// `inputLevelsMul` = [[magnitude (RMS), peak, inputPeak], …по каналам] → RMS в дБ.
fn meter_db(levels: &Value) -> f32 {
    let mul = levels
        .as_array()
        .map(|chs| {
            chs.iter()
                .filter_map(|c| c.get(0).and_then(Value::as_f64))
                .fold(0.0f64, f64::max)
        })
        .unwrap_or(0.0);
    if mul > 0.0 {
        (20.0 * mul.log10()).max(-100.0) as f32
    } else {
        -100.0
    }
}

fn connect(port: u16, url: &str) -> Result<WebSocket<TcpStream>, String> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .map_err(|_| "OBS не запущен".to_string())?;
    tcp.set_read_timeout(Some(Duration::from_secs(1)))
        .map_err(|e| e.to_string())?;
    let (ws, _) = tungstenite::client(url, tcp).map_err(|e| format!("Рукопожатие с OBS: {e}"))?;
    Ok(ws)
}

fn read_json(ws: &mut WebSocket<TcpStream>) -> Result<Value, String> {
    loop {
        match ws.read().map_err(|e| format!("OBS: {e}"))? {
            Message::Text(t) => return serde_json::from_str(t.as_str()).map_err(|e| e.to_string()),
            Message::Close(_) => return Err("OBS закрыл соединение".into()),
            _ => {}
        }
    }
}

fn send_json(ws: &mut WebSocket<TcpStream>, v: &Value) -> Result<(), String> {
    ws.send(Message::text(v.to_string()))
        .map_err(|e| format!("OBS: {e}"))
}

/// base64(sha256(base64(sha256(password + salt)) + challenge)) — по протоколу obs-websocket v5.
fn auth_string(password: &str, salt: &str, challenge: &str) -> String {
    let secret = B64.encode(Sha256::digest(format!("{password}{salt}").as_bytes()));
    B64.encode(Sha256::digest(format!("{secret}{challenge}").as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obs_auth_is_deterministic() {
        let a = auth_string("pass", "salt", "challenge");
        assert_eq!(a, auth_string("pass", "salt", "challenge"));
        assert_ne!(a, auth_string("pass2", "salt", "challenge"));
        assert_eq!(a.len(), 44);
    }

    #[test]
    fn meter_db_takes_loudest_channel_rms() {
        let v = json!([[0.1, 0.5, 0.5], [0.01, 0.2, 0.2]]);
        assert!((meter_db(&v) + 20.0).abs() < 0.01);
        assert_eq!(meter_db(&json!([])), -100.0);
    }
}
