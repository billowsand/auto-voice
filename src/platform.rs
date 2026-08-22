//! Small platform boundary shared by the desktop UI and input pipeline.

use std::path::PathBuf;

use eframe::egui::{Pos2, Vec2};

#[derive(Clone, Debug)]
pub struct Capabilities {
    pub platform_name: &'static str,
    pub session_name: String,
    pub global_ptt: Capability,
    pub overlay_position: Capability,
    pub synthetic_paste: Capability,
    pub permission_hint: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)] // Each target constructs only the variants relevant to that platform.
pub enum Capability {
    Available,
    PermissionRequired,
    Degraded,
}

impl Capability {
    pub fn label(self) -> &'static str {
        match self {
            Self::Available => "可用",
            Self::PermissionRequired => "需要授权",
            Self::Degraded => "受限",
        }
    }
}

pub fn capabilities() -> Capabilities {
    #[cfg(target_os = "windows")]
    {
        Capabilities {
            platform_name: "Windows",
            session_name: "Win32".to_owned(),
            global_ptt: Capability::Available,
            overlay_position: Capability::Available,
            synthetic_paste: Capability::Available,
            permission_hint: None,
        }
    }

    #[cfg(target_os = "macos")]
    {
        Capabilities {
            platform_name: "macOS",
            session_name: "AppKit".to_owned(),
            global_ptt: Capability::PermissionRequired,
            overlay_position: Capability::Available,
            synthetic_paste: Capability::PermissionRequired,
            permission_hint: Some("需要在系统设置中允许麦克风、辅助功能与输入监控。"),
        }
    }

    #[cfg(target_os = "linux")]
    {
        let wayland = is_wayland_session();
        Capabilities {
            platform_name: "Linux",
            session_name: if wayland { "Wayland" } else { "X11" }.to_owned(),
            global_ptt: if wayland {
                Capability::Degraded
            } else {
                Capability::Available
            },
            overlay_position: if wayland {
                Capability::Degraded
            } else {
                Capability::Available
            },
            synthetic_paste: if wayland {
                Capability::Degraded
            } else {
                Capability::Available
            },
            permission_hint: wayland.then_some(
                "当前为 Wayland：全局 PTT 需要 GlobalShortcuts Portal，悬浮窗位置由合成器决定。",
            ),
        }
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        Capabilities {
            platform_name: std::env::consts::OS,
            session_name: "unsupported".to_owned(),
            global_ptt: Capability::Degraded,
            overlay_position: Capability::Degraded,
            synthetic_paste: Capability::Degraded,
            permission_hint: Some("当前平台尚未经过支持验证。"),
        }
    }
}

/// Top-left corner, in physical pixels, where the overlay should pop for the app that currently
/// has focus. `None` means the caller should fall back to the primary monitor.
///
/// When the focused app exposes a Win32 caret the overlay sits right under the insertion point,
/// the way a system IME candidate window does. Apps that draw their own caret (Chromium,
/// Electron, most editors) expose nothing, so we fall back to the bottom of their window.
#[cfg(target_os = "windows")]
pub fn overlay_origin(size: Vec2, follow_caret: bool) -> Option<Pos2> {
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        ClientToScreen, GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, MONITORINFO,
        MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetGUIThreadInfo, GetWindowRect, GetWindowThreadProcessId,
        GUITHREADINFO,
    };

    // A null foreground window (rare, e.g. during a desktop switch) still gets a sensible
    // position rather than `None`, because the overlay is parked off-screen between uses and
    // has to be told where to come back to.
    let foreground = unsafe { GetForegroundWindow() };

    let work_area = unsafe {
        let monitor = if foreground.is_null() {
            MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY)
        } else {
            MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST)
        };
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        (GetMonitorInfoW(monitor, &mut info) != 0).then_some(info.rcWork)?
    };

    let caret = (follow_caret && !foreground.is_null())
        .then(|| unsafe {
            let thread = GetWindowThreadProcessId(foreground, std::ptr::null_mut());
            let mut info: GUITHREADINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
            if GetGUIThreadInfo(thread, &mut info) == 0 || info.hwndCaret.is_null() {
                return None;
            }
            let caret = info.rcCaret;
            if caret.bottom <= caret.top {
                return None;
            }
            let mut origin = POINT {
                x: caret.left,
                y: caret.bottom,
            };
            (ClientToScreen(info.hwndCaret, &mut origin) != 0).then_some(origin)
        })
        .flatten();

    let (x, y) = match caret {
        // Slightly left of and below the insertion point, like an IME candidate bar.
        Some(point) => (point.x as f32 - 18.0, point.y as f32 + 14.0),
        None => {
            let mut rect: RECT = unsafe { std::mem::zeroed() };
            let window = (!foreground.is_null()
                && unsafe { GetWindowRect(foreground, &mut rect) } != 0)
                .then_some(rect);
            let host = window.unwrap_or(work_area);
            (
                (host.left + host.right) as f32 * 0.5 - size.x * 0.5,
                host.bottom as f32 - size.y - 96.0,
            )
        }
    };

    Some(Pos2::new(
        x.clamp(
            work_area.left as f32 + 8.0,
            (work_area.right as f32 - size.x - 8.0).max(work_area.left as f32 + 8.0),
        ),
        y.clamp(
            work_area.top as f32 + 8.0,
            (work_area.bottom as f32 - size.y - 8.0).max(work_area.top as f32 + 8.0),
        ),
    ))
}

#[cfg(not(target_os = "windows"))]
pub fn overlay_origin(_size: Vec2, _follow_caret: bool) -> Option<Pos2> {
    // X11/Wayland/macOS caret probing needs AT-SPI / Accessibility permissions; the overlay
    // falls back to a fixed spot above the taskbar until those backends land.
    None
}

/// Make pure-black pixels of the overlay window transparent and click-through.
///
/// The GL configs glutin picks on Windows report no composition support, so the overlay's
/// alpha channel is ignored and its canvas composites as an opaque black rectangle. Color-key
/// layering is the reliable way out: the overlay clears to pure black, the pill never is, so
/// only the pill remains on screen.
#[cfg(target_os = "windows")]
pub fn punch_out_overlay_background(window_title: &str) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, GetWindowLongPtrW, SetLayeredWindowAttributes, SetWindowLongPtrW, GWL_EXSTYLE,
        LWA_COLORKEY, WS_EX_LAYERED,
    };

    let title: Vec<u16> = window_title
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let window = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
    if window.is_null() {
        return false;
    }
    unsafe {
        let style = GetWindowLongPtrW(window, GWL_EXSTYLE);
        SetWindowLongPtrW(window, GWL_EXSTYLE, style | WS_EX_LAYERED as isize);
        SetLayeredWindowAttributes(window, 0x0000_0000, 0, LWA_COLORKEY) != 0
    }
}

#[cfg(not(target_os = "windows"))]
pub fn punch_out_overlay_background(_window_title: &str) -> bool {
    // Wayland/X11/macOS composite the alpha channel of the GL surface directly.
    true
}

/// Fallback overlay origin, in physical pixels: horizontally centred, above the taskbar.
pub fn overlay_fallback_origin(size: Vec2, monitor: Vec2) -> Pos2 {
    Pos2::new(
        ((monitor.x - size.x) * 0.5).max(8.0),
        (monitor.y - size.y - 110.0).max(8.0),
    )
}

pub fn app_log_dir() -> PathBuf {
    directories::ProjectDirs::from("io.github", "billowsand", "auto-voice")
        .map(|dirs| {
            dirs.state_dir()
                .unwrap_or_else(|| dirs.data_local_dir())
                .join("logs")
        })
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(target_os = "linux")]
pub fn is_wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE")
            .is_ok_and(|value| value.eq_ignore_ascii_case("wayland"))
}

/// Add a system CJK font as a fallback without redistributing an OS font.
pub fn install_system_fonts(ctx: &eframe::egui::Context) {
    let Some(path) = system_cjk_font_candidates()
        .into_iter()
        .find(|path| path.is_file())
    else {
        tracing::warn!("No system CJK font found; Chinese UI glyphs may be unavailable");
        return;
    };

    match std::fs::read(&path) {
        Ok(bytes) => {
            let mut fonts = eframe::egui::FontDefinitions::default();
            fonts.font_data.insert(
                "auto_voice_cjk".to_owned(),
                eframe::egui::FontData::from_owned(bytes).into(),
            );
            for family in [
                eframe::egui::FontFamily::Proportional,
                eframe::egui::FontFamily::Monospace,
            ] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .push("auto_voice_cjk".to_owned());
            }
            ctx.set_fonts(fonts);
            tracing::info!("Loaded UI font fallback from {}", path.display());
        }
        Err(error) => tracing::warn!("Failed to read UI font {}: {error}", path.display()),
    }
}

fn system_cjk_font_candidates() -> Vec<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let windows_dir = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        return ["Noto Sans SC (TrueType).otf", "msyh.ttc", "simhei.ttf"]
            .into_iter()
            .map(|name| windows_dir.join("Fonts").join(name))
            .collect();
    }

    #[cfg(target_os = "macos")]
    {
        return [
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/STHeiti Light.ttc",
            "/Library/Fonts/Arial Unicode.ttf",
        ]
        .into_iter()
        .map(std::path::Path::new)
        .map(std::path::Path::to_path_buf)
        .collect();
    }

    #[cfg(target_os = "linux")]
    {
        return [
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf",
            "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        ]
        .into_iter()
        .map(std::path::Path::new)
        .map(std::path::Path::to_path_buf)
        .collect();
    }

    #[allow(unreachable_code)]
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_labels_are_user_facing() {
        assert_eq!(Capability::Available.label(), "可用");
        assert_eq!(Capability::Degraded.label(), "受限");
    }
}
