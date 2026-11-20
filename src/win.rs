//! Тонкие обёртки над WinAPI: приоритет процесса, сообщения главному потоку, автозапуск,
//! глобальные горячие клавиши.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, SYSTEMTIME};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows_sys::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, CreateMutexW, GetCurrentProcess, GetCurrentThread,
    GetCurrentThreadId, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
    ProcessPowerThrottling, SetPriorityClass, SetProcessInformation, SetThreadPriority,
    THREAD_MODE_BACKGROUND_BEGIN, THREAD_PRIORITY_LOWEST,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    IDYES, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_YESNO, MessageBoxW, PostThreadMessageW, SW_SHOWNORMAL,
    WM_APP,
};

pub const APP_NAME: &str = "Vox";

/// Сообщение-«будильник» для главного потока. Сами события лежат в `PENDING`:
/// пока открыто меню трея, модальный цикл Windows выбрасывает thread-сообщения,
/// а флаг не теряется и обработается на следующем сообщении.
const WM_WAKE: u32 = WM_APP + 1;

/// Состояние поменялось — обновить меню/иконку/захват.
pub const EV_REFRESH: u32 = 1;
/// Аудиопоток упал (устройство пропало).
pub const EV_STREAM_ERROR: u32 = 2;
/// В очереди есть команды от страницы настроек.
pub const EV_CMD: u32 = 4;
/// Двойной щелчок по иконке трея.
pub const EV_OPEN_SETTINGS: u32 = 8;
/// Запущен установщик обновления — выйти.
pub const EV_QUIT: u32 = 16;

static MAIN_THREAD: AtomicU32 = AtomicU32::new(0);
static PENDING: AtomicU32 = AtomicU32::new(0);

pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

pub fn register_main_thread() {
    MAIN_THREAD.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
}

/// Отметить событие и разбудить главный поток (вызывается из любых потоков).
pub fn notify_main(ev: u32) {
    PENDING.fetch_or(ev, Ordering::AcqRel);
    let tid = MAIN_THREAD.load(Ordering::Relaxed);
    if tid != 0 {
        unsafe { PostThreadMessageW(tid, WM_WAKE, 0, 0) };
    }
}

pub fn take_pending() -> u32 {
    PENDING.swap(0, Ordering::AcqRel)
}

/// Пониженный приоритет + EcoQoS (Windows 11 отправляет процесс на энергоэффективные ядра).
pub fn make_process_humble(eco: bool) {
    unsafe {
        SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS);
        if eco {
            let state = PROCESS_POWER_THROTTLING_STATE {
                Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
                StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            };
            SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                &state as *const _ as *const _,
                size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            );
        }
    }
}

pub fn lower_current_thread() {
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST) };
}

/// Фоновый режим потока: самый низкий приоритет процессора, диска и памяти. Такой поток
/// получает только то время, которое никому больше не нужно, а его страницы памяти
/// вытесняются первыми — игре он не мешает.
pub fn background_current_thread() {
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
}

/// `false`, если приложение уже запущено. Один экземпляр на папку настроек:
/// обычно она одна, а для проверки сборки можно запустить копию с другим `%APPDATA%`.
pub fn acquire_single_instance() -> bool {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    crate::config::app_dir()
        .to_string_lossy()
        .to_lowercase()
        .hash(&mut h);
    let name = wide(format!("Local\\VoxSingleInstance-{:016x}", h.finish()));
    unsafe {
        let h = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        // Хэндл намеренно не закрываем: мьютекс живёт до конца процесса.
        !h.is_null() && GetLastError() != ERROR_ALREADY_EXISTS
    }
}

/// Короткий 8.3-путь: sherpa-onnx открывает файлы модели через `char*`, и кириллица в пути
/// (например, в имени пользователя) иначе ломает загрузку.
pub fn short_path(p: &Path) -> String {
    let w = wide(p);
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetShortPathNameW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n >= buf.len() {
        return p.to_string_lossy().into_owned();
    }
    String::from_utf16_lossy(&buf[..n])
}

/// Открыть файл, папку или ссылку программой по умолчанию.
pub fn shell_open(p: impl AsRef<OsStr>) -> bool {
    shell_open_args(p, None)
}

/// Открыть файл или ссылку; программе — с аргументами командной строки.
pub fn shell_open_args(p: impl AsRef<OsStr>, args: Option<&str>) -> bool {
    let op = wide("open");
    let file = wide(p);
    let args = args.map(wide);
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            file.as_ptr(),
            args.as_ref().map_or(std::ptr::null(), |a| a.as_ptr()),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // По документации успех — значение больше 32.
    r as usize > 32
}

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Vox";
/// Имя автозапуска до переименования.
const OLD_RUN_VALUE: &str = "Murlyka";

/// Автозапуск: значение в HKCU\…\Run (его же ставит галочка в установщике).
pub fn autostart_enabled() -> bool {
    let key = wide(RUN_KEY);
    let name = wide(RUN_VALUE);
    let mut size = 0u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    r == 0
}

pub fn set_autostart(on: bool) -> bool {
    let key = wide(RUN_KEY);
    let name = wide(RUN_VALUE);
    let r = if on {
        let Ok(exe) = std::env::current_exe() else {
            return false;
        };
        let data = wide(format!("\"{}\"", exe.display()));
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        }
    } else {
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) }
    };
    r == 0
}

/// Автозапуск старой «Мурлыки» превращается в автозапуск Vox.
pub fn migrate_autostart() {
    let key = wide(RUN_KEY);
    let old = wide(OLD_RUN_VALUE);
    let mut size = 0u32;
    let had = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            old.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    } == 0;
    if had {
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), old.as_ptr()) };
        set_autostart(true);
    }
}

/// Глобальные горячие клавиши Ctrl+Alt+<цифра>. WM_HOTKEY приходит в очередь главного
/// потока (окна у нас нет). Возвращает клавиши, которые занял кто-то другой.
pub fn register_hotkeys(keys: &[(i32, char)]) -> Vec<char> {
    let mut busy = Vec::new();
    for &(id, ch) in keys {
        let ok = unsafe {
            RegisterHotKey(
                std::ptr::null_mut(),
                id,
                MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
                ch as u32,
            )
        };
        if ok == 0 {
            busy.push(ch);
        }
    }
    busy
}

pub fn unregister_hotkeys(keys: &[(i32, char)]) {
    for &(id, _) in keys {
        unsafe { UnregisterHotKey(std::ptr::null_mut(), id) };
    }
}

/// Положить текст в буфер обмена.
pub fn copy_text(text: &str) -> bool {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
    let w = wide(text);
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return false;
        }
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2);
        let mut ok = false;
        if !h.is_null() {
            let p = GlobalLock(h) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
                GlobalUnlock(h);
                // При успехе памятью владеет буфер обмена.
                ok = !SetClipboardData(CF_UNICODETEXT as u32, h).is_null();
            }
        }
        CloseClipboard();
        ok
    }
}

pub fn error_box(text: &str) {
    message_box(text, MB_OK | MB_ICONERROR);
}

pub fn info_box(text: &str) {
    message_box(text, MB_OK | MB_ICONINFORMATION);
}

pub fn ask_yes_no(text: &str) -> bool {
    message_box(text, MB_ICONQUESTION | MB_YESNO) == IDYES
}

fn message_box(text: &str, flags: u32) -> i32 {
    let t = wide(text);
    let c = wide(APP_NAME);
    unsafe { MessageBoxW(std::ptr::null_mut(), t.as_ptr(), c.as_ptr(), flags) }
}

/// Номер текущих суток (дни от 1970-01-01); сутки начинаются в `day_start_hour` по местному времени.
pub fn local_day_key(day_start_hour: u8) -> i64 {
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    let day = days_from_civil(t.wYear as i64, t.wMonth as i64, t.wDay as i64);
    if (t.wHour as u8) < day_start_hour { day - 1 } else { day }
}

/// Дни от 1970-01-01 (алгоритм Хиннанта).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}
