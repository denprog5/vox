//! Локальный сервер на 127.0.0.1: отдаёт аватар (`/`) для источника «Браузер» в OBS,
//! страницу настроек (`/settings`) и держит WebSocket (`/ws`), по которому приложение
//! шлёт состояние кошки. Свой минимальный HTTP/WebSocket: страниц всего две-три,
//! на каждое соединение — поток, который спит в блокирующем чтении.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::Receiver;
use std::thread;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::Value;
use sha1::{Digest, Sha1};

use crate::hub::{Cmd, Kind, SharedHub};

struct Asset {
    path: &'static str,
    mime: &'static str,
    body: &'static [u8],
}

macro_rules! asset {
    ($path:literal, $mime:literal, $file:literal) => {
        Asset {
            path: $path,
            mime: $mime,
            body: include_bytes!(concat!("../assets/web/", $file)),
        }
    };
}

static ASSETS: &[Asset] = &[
    asset!("/", "text/html; charset=utf-8", "avatar.html"),
    asset!("/settings", "text/html; charset=utf-8", "settings.html"),
    asset!("/logo.png", "image/png", "logo.png"),
    asset!("/img/idle.webp", "image/webp", "img/idle.webp"),
    asset!("/img/blink.webp", "image/webp", "img/blink.webp"),
    asset!("/img/talk.webp", "image/webp", "img/talk.webp"),
    asset!("/img/shout.webp", "image/webp", "img/shout.webp"),
    asset!("/img/sad.webp", "image/webp", "img/sad.webp"),
    asset!("/img/sad-talk.webp", "image/webp", "img/sad-talk.webp"),
    asset!("/img/laugh.webp", "image/webp", "img/laugh.webp"),
    asset!("/img/surprise.webp", "image/webp", "img/surprise.webp"),
    asset!("/img/sing.webp", "image/webp", "img/sing.webp"),
    asset!("/img/eat-1.webp", "image/webp", "img/eat-1.webp"),
    asset!("/img/eat-2.webp", "image/webp", "img/eat-2.webp"),
];

/// Запускает сервер. Ошибка — порт занят.
pub fn spawn(port: u16, hub: SharedHub) -> Result<(), String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| format!("Порт {port} занят ({e}). Поменяйте port в config.json"))?;
    thread::Builder::new()
        .name("http".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let hub = hub.clone();
                let _ = thread::Builder::new()
                    .name("conn".into())
                    .stack_size(256 * 1024)
                    .spawn(move || handle(stream, hub, port));
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Запрос пришёл на наш адрес, а не на чужое имя, которое DNS направил на 127.0.0.1.
fn host_ok(host: &str, port: u16) -> bool {
    host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}")
}

/// WebSocket открывает только наша же страница (аватар или настройки). Любой сайт в браузере
/// может подключиться к 127.0.0.1 — браузер тогда шлёт свой Origin, его и отсекаем.
/// Без Origin приходят не браузеры: локальная программа и так может всё.
fn origin_ok(origin: Option<&str>, port: u16) -> bool {
    origin.is_none_or(|o| {
        o == format!("http://127.0.0.1:{port}") || o == format!("http://localhost:{port}")
    })
}

fn handle(stream: TcpStream, hub: SharedHub, port: u16) {
    let _ = stream.set_nodelay(true);
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    let path = target.split('?').next().unwrap_or("/");
    let query = target.split_once('?').map_or("", |(_, q)| q);
    let (mut ws_key, mut host, mut origin) = (None, String::new(), None);
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            let (k, v) = (k.trim(), v.trim().to_string());
            if k.eq_ignore_ascii_case("sec-websocket-key") {
                ws_key = Some(v);
            } else if k.eq_ignore_ascii_case("host") {
                host = v.to_ascii_lowercase();
            } else if k.eq_ignore_ascii_case("origin") {
                origin = Some(v.to_ascii_lowercase());
            }
        }
    }
    let mut out = stream;
    if method != "GET" {
        let _ = respond(&mut out, "405 Method Not Allowed", "text/plain", b"");
        return;
    }
    if !host_ok(&host, port) {
        let _ = respond(&mut out, "403 Forbidden", "text/plain", b"bad host");
        return;
    }
    if path == "/ws" {
        if !origin_ok(origin.as_deref(), port) {
            let _ = respond(&mut out, "403 Forbidden", "text/plain", b"bad origin");
            return;
        }
        if let Some(key) = ws_key {
            let kind = if query.contains("settings") {
                Kind::Settings
            } else {
                Kind::Avatar
            };
            websocket(out, reader, &key, kind, hub);
        }
        return;
    }
    match ASSETS.iter().find(|a| a.path == path) {
        Some(a) => {
            let _ = respond(&mut out, "200 OK", a.mime, a.body);
        }
        None => {
            let _ = respond(&mut out, "404 Not Found", "text/plain", b"not found");
        }
    }
}

fn respond(out: &mut TcpStream, status: &str, mime: &str, body: &[u8]) -> std::io::Result<()> {
    let cache = if mime.starts_with("image/") {
        "max-age=86400"
    } else {
        "no-cache"
    };
    write!(
        out,
        "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nCache-Control: {cache}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    out.write_all(body)?;
    out.flush()
}

// ---------------------------------------------------------------------------------------------
// WebSocket (RFC 6455): только текстовые кадры, ping/pong и close.

fn accept_key(key: &str) -> String {
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    h.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    B64.encode(h.finalize())
}

fn websocket(
    mut out: TcpStream,
    mut reader: BufReader<TcpStream>,
    key: &str,
    kind: Kind,
    hub: SharedHub,
) {
    let head = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        accept_key(key)
    );
    if out.write_all(head.as_bytes()).is_err() {
        return;
    }
    let (id, rx) = hub.add(kind);
    // Писатель — отдельный поток: спит на канале и пишет, когда приложению есть что сказать.
    let writer = match out.try_clone() {
        Ok(w) => w,
        Err(_) => {
            hub.remove(id);
            return;
        }
    };
    let wt = thread::Builder::new()
        .name("ws-out".into())
        .stack_size(128 * 1024)
        .spawn(move || write_loop(writer, rx));
    // Читатель: команды от страницы настроек.
    while let Some((opcode, payload)) = read_frame(&mut reader) {
        match opcode {
            0x1 if kind == Kind::Settings => {
                if let Ok(v) = serde_json::from_slice::<Value>(&payload) {
                    dispatch(&hub, v);
                }
            }
            0x8 => break,
            0x9 => {
                let _ = write_frame(&mut out, 0xA, &payload);
            }
            _ => {}
        }
    }
    hub.remove(id);
    let _ = out.shutdown(std::net::Shutdown::Both);
    if let Ok(t) = wt {
        let _ = t.join();
    }
}

fn dispatch(hub: &SharedHub, v: Value) {
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    match v["c"].as_str() {
        Some("set") => hub.push_cmd(Cmd::Set(v["v"].clone())),
        Some("emo") => hub.push_cmd(Cmd::Emotion(s("e"))),
        Some("train") => hub.push_cmd(Cmd::Train(s("e"))),
        Some("untrain") => hub.push_cmd(Cmd::Untrain(s("e"))),
        Some("mics") => hub.push_cmd(Cmd::RefreshMics),
        Some("swears") if v["a"] == "reset" => hub.push_cmd(Cmd::SwearReset),
        Some("update") if v["a"] == "check" => hub.push_cmd(Cmd::UpdateCheck),
        Some("update") if v["a"] == "install" => hub.push_cmd(Cmd::UpdateInstall),
        _ => {}
    }
}

fn write_loop(mut out: TcpStream, rx: Receiver<String>) {
    // recv() вернёт ошибку, когда клиент удалён из hub (отправитель уничтожен).
    while let Ok(msg) = rx.recv() {
        if write_frame(&mut out, 0x1, msg.as_bytes()).is_err() {
            break;
        }
    }
}

fn write_frame(out: &mut TcpStream, opcode: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut head = Vec::with_capacity(10);
    head.push(0x80 | opcode);
    let n = payload.len();
    if n < 126 {
        head.push(n as u8);
    } else if n <= u16::MAX as usize {
        head.push(126);
        head.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        head.push(127);
        head.extend_from_slice(&(n as u64).to_be_bytes());
    }
    out.write_all(&head)?;
    out.write_all(payload)
}

/// Кадр от браузера (всегда с маской). Фрагментация не нужна: наши сообщения маленькие.
fn read_frame(r: &mut impl Read) -> Option<(u8, Vec<u8>)> {
    let mut h = [0u8; 2];
    r.read_exact(&mut h).ok()?;
    let opcode = h[0] & 0x0F;
    let masked = h[1] & 0x80 != 0;
    let mut len = (h[1] & 0x7F) as u64;
    if len == 126 {
        let mut b = [0u8; 2];
        r.read_exact(&mut b).ok()?;
        len = u16::from_be_bytes(b) as u64;
    } else if len == 127 {
        let mut b = [0u8; 8];
        r.read_exact(&mut b).ok()?;
        len = u64::from_be_bytes(b);
    }
    if len > 1 << 20 {
        return None;
    }
    let mut mask = [0u8; 4];
    if masked {
        r.read_exact(&mut mask).ok()?;
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload).ok()?;
    if masked {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
    }
    Some((opcode, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_accept_matches_rfc_example() {
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn rejects_foreign_hosts_and_origins() {
        assert!(host_ok("127.0.0.1:7710", 7710));
        assert!(host_ok("localhost:7710", 7710));
        assert!(!host_ok("evil.example:7710", 7710));
        assert!(!host_ok("127.0.0.1:7711", 7710));
        assert!(origin_ok(None, 7710));
        assert!(origin_ok(Some("http://127.0.0.1:7710"), 7710));
        assert!(!origin_ok(Some("https://evil.example"), 7710));
        assert!(!origin_ok(Some("null"), 7710));
    }

    #[test]
    fn reads_masked_frame() {
        let payload = b"hello";
        let mask = [1u8, 2, 3, 4];
        let mut f = vec![0x81, 0x80 | payload.len() as u8];
        f.extend_from_slice(&mask);
        f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        let (op, p) = read_frame(&mut &f[..]).unwrap();
        assert_eq!(op, 1);
        assert_eq!(p, payload);
    }
}
