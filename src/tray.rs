/// 系统托盘模式：双击 exe 无参数启动时进入。
///
/// - 托盘图标常驻通知区域
/// - 自动进入 PTT 模式（配置键触发录音）
/// - 录音状态 OSD
/// - 右键菜单 → 退出
use anyhow::Result;
use tray_icon::{
    menu::{Menu, MenuItem},
    TrayIcon, TrayIconBuilder,
};

use crate::audio::ptt;
use crate::config::describe_ptt_key;
use crate::osd;
use crate::runtime::Runtime;

#[cfg(windows)]
use windows_sys::Win32::System::Console::GetConsoleWindow;
#[cfg(windows)]
use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};

pub struct TrayConfig {
    pub runtime: Runtime,
    pub lmstudio: Option<crate::lmstudio::AutoStartConfig>,
    pub settings: crate::config::ConfigFile,
}

pub fn run_tray(cfg: TrayConfig) -> Result<()> {
    #[cfg(windows)]
    hide_console_window();

    // tray-icon uses GTK menus on Linux but does not initialize GTK itself.
    // Do this on the main thread before eframe's callback constructs the menu.
    #[cfg(target_os = "linux")]
    gtk::init().map_err(|error| anyhow::anyhow!("GTK initialization failed: {error}"))?;

    let live = cfg.runtime.live();
    let ptt_key_label = describe_ptt_key(&live.ptt_key);

    // ── OSD 浮动状态窗口 ─────────────────────────────────────────────────────
    let osd_handle = osd::OsdHandle::new();
    osd_handle.set_hotkey_label(ptt_key_label.clone());
    osd_handle.set_follow_caret(live.follow_caret);

    // ── 后台线程：PTT（模型由 run_ptt 自己按需加载/重载）──────────────────────
    {
        let runtime = cfg.runtime.clone();
        let osd_handle = osd_handle.clone();
        let lmstudio_config = cfg.lmstudio;

        std::thread::spawn(move || {
            // LM Studio 起得慢，但只有转写完成后才需要它，所以不阻塞 PTT 就绪。
            if let Some(config) = lmstudio_config {
                std::thread::spawn(move || {
                    if let Err(error) = crate::lmstudio::ensure_ready(&config) {
                        tracing::warn!("LM Studio auto-start failed; LLM calls will fall back to raw text: {error:#}");
                    }
                });
            }

            tracing::info!("PTT active");
            #[cfg(target_os = "linux")]
            let desktop_osd = (!crate::platform::is_wayland_session()).then_some(osd_handle);
            #[cfg(not(target_os = "linux"))]
            let desktop_osd = Some(osd_handle);

            if let Err(e) = ptt::run_ptt(&runtime, desktop_osd) {
                tracing::error!("PTT error: {}", e);
            }
        });
    }

    // ── 主线程：eframe/winit 事件循环 + 设置窗口 + OSD + 托盘 ────────────────
    let settings = cfg.settings;
    // 首次运行才弹窗口走引导；配置过的安装直接静默进托盘。
    let show_window = !settings.is_configured();
    let runtime = cfg.runtime;
    let native_options = crate::ui::native_options(show_window);
    eframe::run_native(
        "auto-voice",
        native_options,
        Box::new(move |creation| {
            let (tray, actions) = build_tray(&ptt_key_label)?;
            Ok(Box::new(crate::ui::DesktopApp::new(
                creation, osd_handle, runtime, settings, tray, actions,
            )))
        }),
    )
    .map_err(|error| anyhow::anyhow!("desktop UI failed: {error}"))?;
    Ok(())
}

// ── 图标、隐藏控制台 ─────────────────────────────────────────────────────────

/// Load the bundled 32×32 RGBA application icon.
fn build_icon() -> tray_icon::Icon {
    const ICON_RGBA: &[u8; 32 * 32 * 4] = include_bytes!("../assets/icons/auto-voice-32.rgba");
    tray_icon::Icon::from_rgba(ICON_RGBA.to_vec(), 32, 32).expect("Failed to create tray icon")
}

fn build_tray(ptt_key_label: &str) -> Result<(TrayIcon, crate::ui::TrayActions)> {
    let settings_item = MenuItem::new("打开设置", true, None);
    let quit_item = MenuItem::new("退出 auto-voice", true, None);
    let menu = Menu::new();
    menu.append(&settings_item)
        .map_err(|error| anyhow::anyhow!("menu error: {error}"))?;
    menu.append(&quit_item)
        .map_err(|error| anyhow::anyhow!("menu error: {error}"))?;

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(format!(
            "Auto Voice  |  按住 {ptt_key_label} 说话，松开后自动插入文字"
        ))
        .with_icon(build_icon())
        .build()
        .map_err(|error| anyhow::anyhow!("tray build failed: {error}"))?;

    Ok((
        tray,
        crate::ui::TrayActions {
            open_settings: settings_item.id().clone(),
            quit: quit_item.id().clone(),
        },
    ))
}

/// Hide the console this process was launched with, if any.
///
/// `FindWindowW(NULL, NULL)` returns whatever top-level window happens to be first in the
/// Z-order, so the previous version of this could hide an unrelated application's window.
#[cfg(windows)]
fn hide_console_window() {
    unsafe {
        let console = GetConsoleWindow();
        if !console.is_null() {
            ShowWindow(console, SW_HIDE);
        }
    }
}
