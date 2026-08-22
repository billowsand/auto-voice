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
        let hyprland = wayland && is_hyprland_session();
        Capabilities {
            platform_name: "Linux",
            session_name: if wayland { "Wayland" } else { "X11" }.to_owned(),
            global_ptt: Capability::Available,
            overlay_position: if wayland && !hyprland {
                Capability::Degraded
            } else {
                Capability::Available
            },
            synthetic_paste: if wayland && !hyprland {
                Capability::Degraded
            } else {
                Capability::Available
            },
            permission_hint: (wayland && !hyprland)
                .then_some("当前为通用 Wayland：需要合成器提供全局 PTT、悬浮窗定位和模拟粘贴。"),
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

#[cfg(target_os = "linux")]
pub fn is_hyprland_session() -> bool {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
        || std::env::var("XDG_CURRENT_DESKTOP")
            .is_ok_and(|desktop| desktop.to_ascii_lowercase().contains("hyprland"))
}

/// Hide the settings window into the tray. Returns false when the window is not known to the
/// compositor yet (only possible during the first frames), so the caller can retry.
///
/// winit's `set_visible` is a no-op on Wayland and Hyprland ignores `xdg_toplevel.set_minimized`,
/// so on Hyprland the window is parked on a dedicated special workspace instead. Elsewhere the
/// minimize request covers compositors that honour it (GNOME, KDE) and X11.
#[cfg(target_os = "linux")]
pub fn hide_main_window(ctx: &eframe::egui::Context) -> bool {
    use eframe::egui::{ViewportCommand, ViewportId};

    if is_hyprland_session() {
        return hyprland_park_main_window();
    }
    if is_wayland_session() {
        ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Minimized(true));
    } else {
        ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Visible(false));
    }
    true
}

/// Bring the settings window back after [`hide_main_window`].
///
/// There is no client-side unminimize on Wayland (winit only forwards the minimize direction),
/// so on Hyprland the window is moved back to the active workspace and focused via hyprctl.
#[cfg(target_os = "linux")]
pub fn show_main_window(ctx: &eframe::egui::Context) {
    use eframe::egui::{ViewportCommand, ViewportId};

    if is_hyprland_session() {
        hyprland_restore_main_window();
        return;
    }
    if !is_wayland_session() {
        ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Visible(true));
        ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Focus);
    }
}

/// Wayland app id set by `ui::native_options`; Hyprland reports it as the window class.
#[cfg(target_os = "linux")]
const HYPRLAND_WINDOW_CLASS: &str = "io.github.billowsand.auto-voice";

/// Hyprland special workspace the settings window is parked on while "in the tray".
#[cfg(target_os = "linux")]
const HYPRLAND_PARK_WORKSPACE: &str = "special:auto-voice";

#[cfg(target_os = "linux")]
fn hyprctl(args: &[&str]) -> bool {
    std::process::Command::new("hyprctl")
        .args(args)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// The `0x…` address Hyprland assigned to the settings window, or `None` while the window has
/// not been mapped yet.
#[cfg(target_os = "linux")]
fn hyprland_main_window_address() -> Option<String> {
    let output = std::process::Command::new("hyprctl")
        .args(["clients", "-j"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let clients: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    clients
        .as_array()?
        .iter()
        .find(|client| {
            client.get("class").and_then(|class| class.as_str()) == Some(HYPRLAND_WINDOW_CLASS)
        })
        .and_then(|client| client.get("address"))
        .and_then(|address| address.as_str())
        .map(str::to_owned)
}

#[cfg(target_os = "linux")]
fn hyprland_active_workspace_id() -> Option<i64> {
    let output = std::process::Command::new("hyprctl")
        .args(["activeworkspace", "-j"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice::<serde_json::Value>(&output.stdout)
        .ok()?
        .get("id")?
        .as_i64()
}

/// Hyprland can run on the Lua config manager (default since the Lua migration), where
/// `hyprctl dispatch` expects a Lua expression (`hl.dsp.…`) instead of the classic
/// `dispatcher args` syntax. Probe once with a no-op and cache the answer.
#[cfg(target_os = "linux")]
fn hyprctl_lua_dispatch() -> bool {
    use std::sync::OnceLock;
    static LUA_DISPATCH: OnceLock<bool> = OnceLock::new();
    *LUA_DISPATCH.get_or_init(|| hyprctl(&["dispatch", "hl.dsp.no_op()"]))
}

#[cfg(target_os = "linux")]
fn hyprland_park_main_window() -> bool {
    let Some(address) = hyprland_main_window_address() else {
        return false;
    };
    let selector = format!("address:{address}");
    if hyprctl_lua_dispatch() {
        hyprctl(&[
            "dispatch",
            &format!(
                "hl.dsp.window.move({{ workspace = \"{HYPRLAND_PARK_WORKSPACE}\", window = \"{selector}\" }})"
            ),
        ])
    } else {
        hyprctl(&[
            "dispatch",
            &format!("movetoworkspacesilent {HYPRLAND_PARK_WORKSPACE},{selector}"),
        ])
    }
}

#[cfg(target_os = "linux")]
fn hyprland_restore_main_window() {
    let Some(address) = hyprland_main_window_address() else {
        return;
    };
    let selector = format!("address:{address}");
    let workspace = hyprland_active_workspace_id();
    if hyprctl_lua_dispatch() {
        if let Some(workspace) = workspace {
            hyprctl(&[
                "dispatch",
                &format!(
                    "hl.dsp.window.move({{ workspace = \"{workspace}\", window = \"{selector}\" }})"
                ),
            ]);
        }
        hyprctl(&[
            "dispatch",
            &format!("hl.dsp.focus({{ window = \"{selector}\" }})"),
        ]);
    } else {
        if let Some(workspace) = workspace {
            hyprctl(&[
                "dispatch",
                &format!("movetoworkspace {workspace},{selector}"),
            ]);
        }
        hyprctl(&["dispatch", &format!("focuswindow {selector}")]);
    }
}

/// Top-left corner, in physical pixels, where the overlay should pop for the app that currently
/// has focus. `None` means the caller should fall back to the primary monitor.
///
/// When the focused app exposes a Win32 caret the overlay sits right under the insertion point,
/// the way a system IME candidate window does. Apps that draw their own caret (Chromium,
/// Electron, most editors) expose nothing, so we fall back to the bottom of their window.
///
/// `card_inset` is the gap between the window's corner and the visible card inside it, so the
/// card rather than the transparent canvas is what gets aligned with the caret.
#[cfg(target_os = "windows")]
pub fn overlay_origin(size: Vec2, card_inset: Vec2, follow_caret: bool) -> Option<Pos2> {
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
        Some(point) => (
            point.x as f32 - 18.0 - card_inset.x,
            point.y as f32 + 14.0 - card_inset.y,
        ),
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

#[cfg(target_os = "linux")]
pub fn overlay_origin(size: Vec2, card_inset: Vec2, follow_caret: bool) -> Option<Pos2> {
    if !is_wayland_session() || !follow_caret {
        return None;
    }

    // Wayland intentionally does not expose another application's text caret. Hyprland does
    // expose the pointer position, which is the closest compositor-wide anchor available and
    // keeps the OSD beside the user's current insertion target instead of inside Settings.
    let output = std::process::Command::new("hyprctl")
        .args(["cursorpos", "-j"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let x = value.get("x")?.as_f64()? as f32;
    let y = value.get("y")?.as_f64()? as f32;
    Some(Pos2::new(
        x + 14.0 - card_inset.x,
        y + 22.0 - card_inset.y - size.y * 0.15,
    ))
}

#[cfg(all(not(target_os = "windows"), not(target_os = "linux")))]
pub fn overlay_origin(_size: Vec2, _card_inset: Vec2, _follow_caret: bool) -> Option<Pos2> {
    None
}

/// How the compositor is told which parts of the overlay window are see-through.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OverlayCompositing {
    /// The alpha channel is blended per pixel. Antialiased edges and real translucency both
    /// work, so the overlay can be drawn like any other rounded, layered surface.
    PerPixelAlpha,
    /// One colour is punched out wholesale. Transparency is 1-bit: every antialiased edge pixel
    /// is either fully opaque or gone, which is why rounded corners come out ragged.
    ColorKey,
}

impl OverlayCompositing {
    /// True when the overlay may rely on partial alpha — translucent fills, soft shadows,
    /// feathered edges. Under a colour key all of those turn into hard black fringes.
    pub fn blends(self) -> bool {
        self == Self::PerPixelAlpha
    }
}

/// Set the overlay window up so its transparent pixels really are transparent.
///
/// Returns `None` while the native window does not exist yet, so the caller can retry on the
/// next frame; the window is only created once the viewport has been shown for the first time.
///
/// DWM blends a window's alpha channel per pixel once blur-behind is enabled with an empty
/// region — the standard recipe for a transparent OpenGL window on Windows, and what gives the
/// overlay smooth corners. `AUTO_VOICE_OVERLAY=colorkey` falls back to the old 1-bit colour key
/// for the rare driver that composites the alpha channel as opaque black.
#[cfg(target_os = "windows")]
pub fn prepare_overlay_window(window_title: &str) -> Option<OverlayCompositing> {
    use windows_sys::Win32::Graphics::Dwm::{
        DwmEnableBlurBehindWindow, DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE,
        DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DWM_BB_BLURREGION, DWM_BB_ENABLE,
        DWM_BLURBEHIND,
    };
    use windows_sys::Win32::Graphics::Gdi::{CreateRectRgn, DeleteObject, SetWindowRgn};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, GetWindowRect, SetLayeredWindowAttributes, SetWindowLongPtrW,
        SetWindowPos, GWL_EXSTYLE, GWL_STYLE, LWA_COLORKEY, SWP_FRAMECHANGED, SWP_NOACTIVATE,
        SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WS_BORDER, WS_CAPTION, WS_EX_LAYERED,
        WS_EX_WINDOWEDGE,
    };

    let window = find_own_window(window_title)?;

    if overlay_mode_override().as_deref() == Some("colorkey") {
        unsafe {
            let style = GetWindowLongPtrW(window, GWL_EXSTYLE);
            SetWindowLongPtrW(window, GWL_EXSTYLE, style | WS_EX_LAYERED as isize);
            SetLayeredWindowAttributes(window, 0x0000_0000, 0, LWA_COLORKEY);
        }
        return Some(OverlayCompositing::ColorKey);
    }

    // winit marks the window `WS_EX_LAYERED` to make it click-through, and a layered window is
    // composited from its colour key / constant alpha rather than from the alpha channel. The
    // click-through itself comes from `WS_EX_TRANSPARENT`, which is left in place.
    unsafe {
        let style = GetWindowLongPtrW(window, GWL_EXSTYLE);
        if style & WS_EX_LAYERED as isize != 0 {
            SetWindowLongPtrW(window, GWL_EXSTYLE, style & !(WS_EX_LAYERED as isize));
        }

        // Blur-behind over an empty region is the standard way to ask DWM to blend a window's
        // alpha channel per pixel. Nothing is actually blurred; the region is empty.
        let region = CreateRectRgn(0, 0, -1, -1);
        let blur = DWM_BLURBEHIND {
            dwFlags: DWM_BB_ENABLE | DWM_BB_BLURREGION,
            fEnable: 1,
            hRgnBlur: region,
            fTransitionOnMaximized: 0,
        };
        let result = DwmEnableBlurBehindWindow(window, &blur);
        DeleteObject(region);
        if result < 0 {
            tracing::warn!("DwmEnableBlurBehindWindow failed (0x{result:08X}); using colour key");
            let style = GetWindowLongPtrW(window, GWL_EXSTYLE);
            SetWindowLongPtrW(window, GWL_EXSTYLE, style | WS_EX_LAYERED as isize);
            SetLayeredWindowAttributes(window, 0x0000_0000, 0, LWA_COLORKEY);
            return Some(OverlayCompositing::ColorKey);
        }

        // winit keeps `WS_CAPTION | WS_BORDER` on every window — undecorated ones included, so
        // that aero snap keeps working — and DWM draws a frame and a drop shadow around any
        // window that has them. Around a transparent canvas that reads as a ghost rectangle
        // hanging in mid-air. A layered window never got either, which is why the colour-key
        // build looked clean; without the layer they have to be taken off explicitly.
        let style = GetWindowLongPtrW(window, GWL_STYLE);
        SetWindowLongPtrW(
            window,
            GWL_STYLE,
            style & !((WS_CAPTION | WS_BORDER) as isize),
        );
        let style_ex = GetWindowLongPtrW(window, GWL_EXSTYLE);
        SetWindowLongPtrW(window, GWL_EXSTYLE, style_ex & !(WS_EX_WINDOWEDGE as isize));

        let corners = DWMWCP_DONOTROUND;
        DwmSetWindowAttribute(
            window,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            std::ptr::addr_of!(corners).cast(),
            size_of_val(&corners) as u32,
        );
        // Windows 11 only; older builds simply reject it.
        let border = DWMWA_COLOR_NONE;
        DwmSetWindowAttribute(
            window,
            DWMWA_BORDER_COLOR as u32,
            std::ptr::addr_of!(border).cast(),
            size_of_val(&border) as u32,
        );

        SetWindowPos(
            window,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
        );

        // Windows still runs a hairline along the top of a borderless window. Nothing is drawn
        // in the outermost pixel of the canvas, so clipping it away costs nothing and takes the
        // line with it.
        let mut rect: windows_sys::Win32::Foundation::RECT = std::mem::zeroed();
        if GetWindowRect(window, &mut rect) != 0 {
            let region =
                CreateRectRgn(1, 1, rect.right - rect.left - 1, rect.bottom - rect.top - 1);
            // The window owns the region from here on; it must not be deleted.
            SetWindowRgn(window, region, 1);
        }

        make_click_through(window);
    }

    Some(OverlayCompositing::PerPixelAlpha)
}

/// Let every mouse event fall through the overlay to the app underneath.
///
/// `WS_EX_TRANSPARENT` — which is how winit implements mouse passthrough — only takes windows
/// out of hit-testing while they are also layered, and the layer is exactly what had to go for
/// the alpha channel to be composited. Answering `WM_NCHITTEST` with `HTTRANSPARENT` is the
/// same statement made directly, and it does not depend on how the window is composited.
#[cfg(target_os = "windows")]
fn make_click_through(window: windows_sys::Win32::Foundation::HWND) {
    use std::sync::atomic::{AtomicIsize, Ordering};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallWindowProcW, DefWindowProcW, GetWindowLongPtrW, SetWindowLongPtrW, GWLP_WNDPROC,
        HTTRANSPARENT, MA_NOACTIVATE, WM_MOUSEACTIVATE, WM_NCHITTEST,
    };

    /// The window procedure winit installed, which handles everything except the two messages
    /// below. There is only ever one overlay window.
    static INNER: AtomicIsize = AtomicIsize::new(0);

    unsafe extern "system" fn proc(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            WM_NCHITTEST => HTTRANSPARENT as LRESULT,
            // Clicking the overlay must not pull focus away from the app being dictated into.
            WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
            _ => match INNER.load(Ordering::Relaxed) {
                0 => unsafe { DefWindowProcW(window, message, wparam, lparam) },
                inner => unsafe {
                    let inner: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT =
                        std::mem::transmute(inner);
                    CallWindowProcW(Some(inner), window, message, wparam, lparam)
                },
            },
        }
    }

    let installed =
        proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT as usize as isize;
    unsafe {
        let current = GetWindowLongPtrW(window, GWLP_WNDPROC);
        if current == installed {
            return;
        }
        INNER.store(current, Ordering::Relaxed);
        SetWindowLongPtrW(window, GWLP_WNDPROC, installed);
    }
}

/// Find this process's window with the given title.
///
/// `FindWindowW` searches every process, so with a second copy of auto-voice running — or the
/// packaged build alongside a development one — it happily hands back the *other* instance's
/// overlay and leaves ours uncomposited.
#[cfg(target_os = "windows")]
fn find_own_window(window_title: &str) -> Option<windows_sys::Win32::Foundation::HWND> {
    use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextW, GetWindowThreadProcessId,
    };

    struct Search {
        title: Vec<u16>,
        process: u32,
        found: HWND,
    }

    unsafe extern "system" fn visit(window: HWND, param: LPARAM) -> BOOL {
        let search = unsafe { &mut *(param as *mut Search) };
        let mut process = 0u32;
        unsafe { GetWindowThreadProcessId(window, &mut process) };
        if process != search.process {
            return 1; // keep enumerating
        }
        let mut buffer = [0u16; 160];
        let length =
            unsafe { GetWindowTextW(window, buffer.as_mut_ptr(), buffer.len() as i32) } as usize;
        if length > 0 && buffer[..length] == search.title[..] {
            search.found = window;
            return 0; // stop
        }
        1
    }

    let mut search = Search {
        title: window_title.encode_utf16().collect(),
        process: unsafe { GetCurrentProcessId() },
        found: std::ptr::null_mut(),
    };
    unsafe { EnumWindows(Some(visit), &mut search as *mut Search as LPARAM) };
    (!search.found.is_null()).then_some(search.found)
}

#[cfg(not(target_os = "windows"))]
pub fn prepare_overlay_window(_window_title: &str) -> Option<OverlayCompositing> {
    // Wayland/X11/macOS composite the alpha channel of the GL surface directly.
    Some(OverlayCompositing::PerPixelAlpha)
}

#[cfg(target_os = "windows")]
fn overlay_mode_override() -> Option<String> {
    std::env::var("AUTO_VOICE_OVERLAY")
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
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

#[derive(Debug, Default)]
pub struct FontLoadReport {
    pub loaded: usize,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct SystemFont {
    pub family: String,
    path: PathBuf,
    face_index: u32,
}

/// Discover installed TrueType/OpenType families. Collections are expanded and the regular face
/// is preferred when a family has several styles.
pub fn list_system_fonts() -> Vec<SystemFont> {
    use std::collections::BTreeMap;

    use skrifa::{raw::FileRef, string::StringId, MetadataProvider};

    let mut families: BTreeMap<String, (SystemFont, u8)> = BTreeMap::new();
    for path in system_font_files() {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(file) = FileRef::new(&bytes) else {
            continue;
        };
        for font in file.fonts().flatten() {
            let family = font
                .localized_strings(StringId::TYPOGRAPHIC_FAMILY_NAME)
                .english_or_first()
                .or_else(|| {
                    font.localized_strings(StringId::FAMILY_NAME)
                        .english_or_first()
                })
                .map(|name| name.to_string())
                .filter(|name| !name.trim().is_empty());
            let Some(family) = family else { continue };
            let style = font
                .localized_strings(StringId::TYPOGRAPHIC_SUBFAMILY_NAME)
                .english_or_first()
                .or_else(|| {
                    font.localized_strings(StringId::SUBFAMILY_NAME)
                        .english_or_first()
                })
                .map(|name| name.to_string())
                .unwrap_or_default();
            let rank = regular_style_rank(&style);
            let key = family.to_lowercase();
            let candidate = SystemFont {
                family,
                path: path.clone(),
                face_index: font.ttc_index().unwrap_or(0),
            };
            match families.entry(key) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert((candidate, rank));
                }
                std::collections::btree_map::Entry::Occupied(mut entry) if rank < entry.get().1 => {
                    entry.insert((candidate, rank));
                }
                _ => {}
            }
        }
    }

    let fonts = families
        .into_values()
        .map(|(font, _)| font)
        .collect::<Vec<_>>();
    tracing::info!("Discovered {} system font families", fonts.len());
    fonts
}

/// Replace the UI's primary fonts with the selected system families, then retain the bundled egui
/// fonts and a system CJK font as fallbacks. An empty selection restores the defaults.
pub fn install_ui_fonts(
    ctx: &eframe::egui::Context,
    selected_families: Option<&[String]>,
    system_fonts: &[SystemFont],
) -> FontLoadReport {
    use eframe::egui::{FontData, FontDefinitions, FontFamily};

    let mut fonts = FontDefinitions::default();
    let mut custom_names = Vec::new();
    let mut report = FontLoadReport::default();

    for (index, family) in selected_families.unwrap_or_default().iter().enumerate() {
        let family = family.trim();
        if family.is_empty() {
            continue;
        }
        let Some(source) = system_fonts
            .iter()
            .find(|font| font.family.eq_ignore_ascii_case(family))
        else {
            report.errors.push(format!("{family}：系统中未找到该字体"));
            continue;
        };
        match std::fs::read(&source.path) {
            Ok(bytes) => {
                let name = format!("auto_voice_custom_{index}");
                let mut data = FontData::from_owned(bytes);
                data.index = source.face_index;
                fonts.font_data.insert(name.clone(), data.into());
                custom_names.push(name);
                report.loaded += 1;
                tracing::info!(
                    "Loaded system UI font {} from {} (face {})",
                    family,
                    source.path.display(),
                    source.face_index
                );
            }
            Err(error) => {
                report.errors.push(format!("{family}：{error}"));
                tracing::warn!(
                    "Failed to read system UI font {} from {}: {error}",
                    family,
                    source.path.display()
                );
            }
        }
    }

    let system_fallback = system_cjk_font_candidates()
        .into_iter()
        .find(|path| path.is_file())
        .and_then(|path| match std::fs::read(&path) {
            Ok(bytes) => {
                let name = "auto_voice_cjk".to_owned();
                fonts
                    .font_data
                    .insert(name.clone(), FontData::from_owned(bytes).into());
                tracing::info!("Loaded UI font fallback from {}", path.display());
                Some(name)
            }
            Err(error) => {
                tracing::warn!("Failed to read UI font {}: {error}", path.display());
                None
            }
        });

    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        let family_fonts = fonts.families.entry(family).or_default();
        for name in custom_names.iter().rev() {
            family_fonts.insert(0, name.clone());
        }
        if let Some(name) = &system_fallback {
            family_fonts.push(name.clone());
        }
    }
    if system_fallback.is_none() {
        tracing::warn!("No system CJK font found; Chinese UI glyphs may be unavailable");
    }

    ctx.set_fonts(fonts);
    report
}

fn regular_style_rank(style: &str) -> u8 {
    let style = style.trim().to_lowercase();
    if matches!(style.as_str(), "regular" | "normal" | "book" | "roman") {
        0
    } else if style.contains("regular") || style.contains("normal") {
        1
    } else {
        2
    }
}

fn is_supported_font_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "ttf" | "otf" | "ttc" | "otc"
            )
        })
}

fn system_font_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = system_font_directories();
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        let mut entries = entries.flatten().collect::<Vec<_>>();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file() && is_supported_font_path(&path) {
                files.push(path);
            }
        }
    }
    files
}

fn system_font_directories() -> Vec<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let windows_dir = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let mut directories = vec![windows_dir.join("Fonts")];
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            directories.push(PathBuf::from(local_app_data).join(r"Microsoft\Windows\Fonts"));
        }
        return directories;
    }

    #[cfg(target_os = "macos")]
    {
        let mut directories = vec![
            PathBuf::from("/System/Library/Fonts"),
            PathBuf::from("/Library/Fonts"),
        ];
        if let Some(home) = std::env::var_os("HOME") {
            directories.push(PathBuf::from(home).join("Library/Fonts"));
        }
        return directories;
    }

    #[cfg(target_os = "linux")]
    {
        let mut directories = vec![
            PathBuf::from("/usr/share/fonts"),
            PathBuf::from("/usr/local/share/fonts"),
        ];
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            directories.push(home.join(".local/share/fonts"));
            directories.push(home.join(".fonts"));
        }
        return directories;
    }

    #[allow(unreachable_code)]
    Vec::new()
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

    #[test]
    fn custom_font_extensions_are_case_insensitive() {
        assert!(is_supported_font_path(std::path::Path::new("ui.ttf")));
        assert!(is_supported_font_path(std::path::Path::new("ui.OTF")));
        assert!(is_supported_font_path(std::path::Path::new("ui.TtC")));
        assert!(is_supported_font_path(std::path::Path::new("ui.otc")));
        assert!(!is_supported_font_path(std::path::Path::new("ui.woff2")));
        assert!(!is_supported_font_path(std::path::Path::new("ui")));
    }

    #[test]
    fn regular_faces_are_preferred_for_family_selection() {
        assert!(regular_style_rank("Regular") < regular_style_rank("Bold"));
        assert!(regular_style_rank("Normal") < regular_style_rank("Italic"));
    }
}
