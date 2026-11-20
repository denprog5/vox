//! Иконки трея — ресурсы exe (`assets/app.rc`): котик, серый на паузе, с жёлтой точкой
//! при загрузке и с красным «!» при ошибке. Windows сама берёт кадр нужного размера.

use tray_icon::Icon;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSMICON};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Listening,
    Paused,
    Busy,
    Error,
}

pub fn make(kind: Kind) -> Icon {
    let id = match kind {
        Kind::Listening => 1,
        Kind::Paused => 2,
        Kind::Busy => 3,
        Kind::Error => 4,
    };
    let n = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16) as u32;
    Icon::from_resource(id, Some((n, n))).expect("иконка в ресурсах exe")
}
