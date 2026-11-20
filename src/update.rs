//! Обновления с GitHub Releases. Поток почти всё время спит: проверка через 15 с после запуска
//! и потом раз в 6 часов — один HTTPS-запрос через системный WinHTTP (без своих TLS-библиотек).
//!
//! Найденное при запуске обновление ставится само (`auto_update`): программа обычно стартует
//! вместе с Windows или до стрима. Найденное позже только показывается в меню трея и на
//! странице настроек — перезапуск посреди трансляции никому не нужен.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::{self, Thread};
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};
use windows_sys::Win32::Networking::WinHttp::{
    INTERNET_DEFAULT_HTTPS_PORT, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle, WinHttpConnect,
    WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
    WinHttpSendRequest, WinHttpSetTimeouts,
};

use crate::config::exe_dir;
use crate::hub::SharedHub;
use crate::win;

pub const REPO: &str = "denprog5/vox";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const FIRST_CHECK: Duration = Duration::from_secs(15);
const INTERVAL: Duration = Duration::from_secs(6 * 3600);

#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    pub page: String,
    /// Установщик: облегчённый (без модели), если программа уже установлена.
    asset_url: Option<String>,
    asset_name: String,
    sha256: Option<String>,
}

/// Версия найденного обновления (для меню трея).
static FOUND: Mutex<Option<String>> = Mutex::new(None);

pub fn found() -> Option<String> {
    FOUND.lock().unwrap().clone()
}

#[derive(Default)]
struct Shared {
    /// Пользователь нажал «Проверить обновления».
    manual: bool,
    /// Пользователь согласился поставить уже найденное обновление.
    install: bool,
    auto: bool,
}

pub struct Updater {
    shared: Arc<Mutex<Shared>>,
    thread: Thread,
}

impl Updater {
    pub fn spawn(auto: bool, hub: SharedHub) -> Updater {
        let shared = Arc::new(Mutex::new(Shared {
            auto,
            ..Default::default()
        }));
        let s = shared.clone();
        let handle = thread::Builder::new()
            .name("update".into())
            .spawn(move || run(s, hub))
            .expect("spawn update");
        Updater {
            shared,
            thread: handle.thread().clone(),
        }
    }

    pub fn set_auto(&self, auto: bool) {
        self.shared.lock().unwrap().auto = auto;
    }

    pub fn check_now(&self) {
        self.shared.lock().unwrap().manual = true;
        self.thread.unpark();
    }

    pub fn install_found(&self) {
        self.shared.lock().unwrap().install = true;
        self.thread.unpark();
    }
}

/// Программа поставлена установщиком (рядом лежит деинсталлятор Inno Setup) —
/// значит, обновление можно поставить тихо поверх.
pub fn installed() -> bool {
    exe_dir().join("unins000.exe").exists()
}

fn run(shared: Arc<Mutex<Shared>>, hub: SharedHub) {
    win::lower_current_thread();
    publish(&hub, None, None);
    let mut found: Option<Release> = None;
    let mut first = true;
    thread::park_timeout(FIRST_CHECK);
    loop {
        let (manual, install, auto) = {
            let mut s = shared.lock().unwrap();
            let r = (s.manual, s.install, s.auto);
            s.manual = false;
            s.install = false;
            r
        };

        if install && let Some(rel) = &found {
            install_or_open(rel);
        } else {
            if manual {
                publish(&hub, found.as_ref().map(|r| r.version.as_str()), Some("проверяю…"));
            }
            let res = latest();
            #[cfg(debug_assertions)]
            eprintln!("[update] {res:?}");
            match res {
                Ok(Some(rel)) => {
                    let ask = manual
                        && win::ask_yes_no(&format!(
                            "Вышла версия {} (у вас {VERSION}). Установить сейчас?\n\n\
                             Vox перезапустится, настройки и счёт сохранятся.",
                            rel.version
                        ));
                    *FOUND.lock().unwrap() = Some(rel.version.clone());
                    publish(&hub, Some(&rel.version), None);
                    win::notify_main(win::EV_REFRESH);
                    if ask || (first && auto && installed()) {
                        install_or_open(&rel);
                    }
                    found = Some(rel);
                }
                Ok(None) => {
                    publish(&hub, None, Some("это последняя версия"));
                    if manual {
                        win::info_box(&format!("У вас последняя версия — {VERSION}."));
                    }
                }
                Err(e) => {
                    let f = found.as_ref().map(|r| r.version.as_str());
                    publish(&hub, f, Some("не удалось проверить"));
                    if manual {
                        win::error_box(&format!("Не удалось проверить обновления:\n{e}"));
                    }
                }
            }
        }
        first = false;
        thread::park_timeout(INTERVAL);
    }
}

/// Состояние для страницы настроек: версия, найденное обновление, итог последней проверки.
fn publish(hub: &SharedHub, found: Option<&str>, note: Option<&str>) {
    let msg = serde_json::json!({
        "t": "update",
        "version": VERSION,
        "found": found,
        "note": note,
    });
    hub.sticky("update", msg.to_string());
}

fn install_or_open(rel: &Release) {
    if !installed() || rel.asset_url.is_none() {
        win::shell_open(&rel.page);
        return;
    }
    match download(rel) {
        Ok(setup) => {
            let args = "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /RELAUNCH=1";
            if win::shell_open_args(&setup, Some(args)) {
                // Установщик сам закроет программу, но лучше уйти самим и убрать иконку из трея.
                win::notify_main(win::EV_QUIT);
            } else {
                win::error_box("Не удалось запустить установщик обновления.");
            }
        }
        Err(e) => win::error_box(&format!("Не удалось скачать обновление:\n{e}")),
    }
}

fn download(rel: &Release) -> Result<PathBuf, String> {
    let url = rel.asset_url.as_deref().unwrap_or_default();
    let data = http_get(url)?;
    if let Some(want) = &rel.sha256 {
        let got = Sha256::digest(&data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        if !got.eq_ignore_ascii_case(want) {
            return Err("контрольная сумма файла не совпала".into());
        }
    }
    let path = std::env::temp_dir().join(&rel.asset_name);
    std::fs::write(&path, &data).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// `Ok(None)` — новее нет.
fn latest() -> Result<Option<Release>, String> {
    let body = http_get(&format!(
        "https://api.github.com/repos/{REPO}/releases/latest"
    ))?;
    let v: Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    Ok(parse_release(&v, installed()).filter(|r| is_newer(&r.version, VERSION)))
}

fn parse_release(v: &Value, installed: bool) -> Option<Release> {
    let version = v["tag_name"].as_str()?.trim_start_matches('v').to_string();
    let assets = v["assets"].as_array().cloned().unwrap_or_default();
    let pick = |prefix: &str| {
        assets.iter().find(|a| {
            a["name"]
                .as_str()
                .is_some_and(|n| n.starts_with(prefix) && n.ends_with(".exe"))
        })
    };
    // Облегчённый установщик без модели подходит только поверх уже установленной программы.
    let asset = if installed {
        pick("Vox-Update-").or_else(|| pick("Vox-Setup-"))
    } else {
        None
    };
    Some(Release {
        page: v["html_url"].as_str().unwrap_or_default().to_string(),
        asset_url: asset.and_then(|a| a["browser_download_url"].as_str().map(String::from)),
        asset_name: asset
            .and_then(|a| a["name"].as_str())
            .unwrap_or("Vox-Update.exe")
            .to_string(),
        sha256: asset
            .and_then(|a| a["digest"].as_str())
            .and_then(|d| d.strip_prefix("sha256:"))
            .map(String::from),
        version,
    })
}

fn is_newer(candidate: &str, current: &str) -> bool {
    let parse =
        |s: &str| -> Vec<u64> { s.split(['.', '-']).map_while(|p| p.parse().ok()).collect() };
    parse(candidate) > parse(current)
}

struct Handle(*mut core::ffi::c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

/// GET по HTTPS через WinHTTP: системные прокси, сертификаты и редиректы (GitHub отдаёт файлы
/// релиза через редирект на свой CDN).
fn http_get(url: &str) -> Result<Vec<u8>, String> {
    let rest = url.strip_prefix("https://").ok_or("нужен https")?;
    let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let path = if path.is_empty() { "/" } else { path };
    let err = |what: &str| format!("{what}: ошибка {}", std::io::Error::last_os_error());

    let agent = win::wide(format!("Vox/{VERSION}"));
    let session = Handle(unsafe {
        WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            std::ptr::null(),
            std::ptr::null(),
            0,
        )
    });
    if session.0.is_null() {
        return Err(err("WinHttpOpen"));
    }
    unsafe { WinHttpSetTimeouts(session.0, 10_000, 10_000, 30_000, 30_000) };
    let host_w = win::wide(host);
    let conn = Handle(unsafe {
        WinHttpConnect(session.0, host_w.as_ptr(), INTERNET_DEFAULT_HTTPS_PORT, 0)
    });
    if conn.0.is_null() {
        return Err(err("подключение"));
    }
    let verb = win::wide("GET");
    let path_w = win::wide(path);
    let req = Handle(unsafe {
        WinHttpOpenRequest(
            conn.0,
            verb.as_ptr(),
            path_w.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        )
    });
    if req.0.is_null() {
        return Err(err("запрос"));
    }
    let headers = win::wide("Accept: application/vnd.github+json, application/octet-stream");
    let ok = unsafe {
        WinHttpSendRequest(
            req.0,
            headers.as_ptr(),
            u32::MAX, // строка заканчивается нулём
            std::ptr::null(),
            0,
            0,
            0,
        ) != 0
            && WinHttpReceiveResponse(req.0, std::ptr::null_mut()) != 0
    };
    if !ok {
        return Err(err("нет связи с GitHub"));
    }
    let mut code = 0u32;
    let mut size = 4u32;
    unsafe {
        WinHttpQueryHeaders(
            req.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            std::ptr::null(),
            (&mut code as *mut u32).cast(),
            &mut size,
            std::ptr::null_mut(),
        )
    };
    if code != 200 {
        return Err(format!("GitHub ответил {code}"));
    }
    let mut body = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let mut read = 0u32;
        if unsafe { WinHttpReadData(req.0, buf.as_mut_ptr().cast(), buf.len() as u32, &mut read) }
            == 0
        {
            return Err(err("чтение ответа"));
        }
        if read == 0 {
            return Ok(body);
        }
        body.extend_from_slice(&buf[..read as usize]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn update_versions_compare() {
        assert!(is_newer("1.0.1", "1.0.0"));
        assert!(is_newer("1.10.0", "1.9.3"));
        assert!(is_newer("2.0.0", "1.99.99"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("0.9.0", "1.0.0"));
    }

    #[test]
    fn update_picks_light_installer() {
        let v = json!({
            "tag_name": "v1.2.0",
            "html_url": "https://github.com/x/y/releases/tag/v1.2.0",
            "assets": [
                {"name": "Vox-Setup-1.2.0.exe", "browser_download_url": "https://a/setup", "digest": "sha256:aa"},
                {"name": "Vox-Update-1.2.0.exe", "browser_download_url": "https://a/update", "digest": "sha256:bb"}
            ]
        });
        let r = parse_release(&v, true).unwrap();
        assert_eq!(r.version, "1.2.0");
        assert_eq!(r.asset_url.as_deref(), Some("https://a/update"));
        assert_eq!(r.sha256.as_deref(), Some("bb"));
        // Не установлено — ставить нечего, только открыть страницу релиза.
        assert!(parse_release(&v, false).unwrap().asset_url.is_none());
    }
}
