/// 系统托盘模式：双击 exe 无参数启动时进入。
///
/// - 托盘图标常驻通知区域
/// - 自动进入 PTT 模式（配置键触发录音）
/// - 录音状态 OSD
/// - 右键菜单 → 退出
use anyhow::Result;
use std::time::Duration;

use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem},
    TrayIconBuilder,
};

use crate::audio::mic::LiveConfig;
use crate::audio::ptt;
use crate::osd;

#[cfg(windows)]
use windows_sys::Win32::Foundation::HWND;
#[cfg(windows)]
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, FindWindowW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, SW_HIDE, WM_QUIT,
};

pub struct TrayConfig {
    pub live_cfg: LiveConfig,
    pub asr_config: crate::asr::AsrConfig,
    pub hr_config: crate::asr::HrConfig,
    pub ptt_key: Option<String>,
}

pub fn run_tray(cfg: TrayConfig) -> Result<()> {
    #[cfg(windows)]
    hide_console_window();

    // ── 托盘图标 ─────────────────────────────────────────────────────────────
    let icon = build_icon();

    // ── 右键菜单 ─────────────────────────────────────────────────────────────
    let quit_item = MenuItem::new("退出 auto-voice", true, None);
    let menu = Menu::new();
    menu.append(&quit_item)
        .map_err(|e| anyhow::anyhow!("menu error: {}", e))?;

    let ptt_key_label = cfg.ptt_key.as_deref().unwrap_or("CapsLock");

    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(format!(
            "auto-voice  |  按住 [{}] 讲话，松开后自动粘贴",
            ptt_key_label
        ))
        .with_icon(icon)
        .build()
        .map_err(|e| anyhow::anyhow!("tray build failed: {}", e))?;

    // ── OSD 浮动状态窗口 ─────────────────────────────────────────────────────
    let osd_handle = osd::spawn_osd();

    // ── 后台线程：加载模型 + 运行 PTT ────────────────────────────────────────
    let asr_config = cfg.asr_config;
    let hr_config = cfg.hr_config;
    let mut live = cfg.live_cfg;
    live.osd = Some(osd_handle);

    std::thread::spawn(
        move || match crate::asr::AsrEngine::new(&asr_config, Some(&hr_config)) {
            Ok(engine) => {
                tracing::info!("ASR model loaded, PTT active");
                if let Err(e) = ptt::run_ptt(&live, &engine) {
                    tracing::error!("PTT error: {}", e);
                }
            }
            Err(e) => {
                tracing::error!("Failed to load ASR model: {}", e);
            }
        },
    );

    // ── 主线程：消息泵 + 托盘事件循环 ───────────────────────────────────────
    let quit_id = quit_item.id().clone();

    loop {
        // 右键菜单事件
        if let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == quit_id {
                std::process::exit(0);
            }
        }

        // Windows 消息泵（托盘事件）
        #[cfg(windows)]
        pump_windows_messages();

        std::thread::sleep(Duration::from_millis(16)); // ~60 fps 响应
    }
}

// ── 图标、隐藏控制台 ─────────────────────────────────────────────────────────

/// Load the bundled 32×32 RGBA application icon.
fn build_icon() -> tray_icon::Icon {
    const ICON_RGBA: &[u8; 32 * 32 * 4] = include_bytes!("../assets/icons/auto-voice-32.rgba");
    tray_icon::Icon::from_rgba(ICON_RGBA.to_vec(), 32, 32).expect("Failed to create tray icon")
}

#[cfg(windows)]
fn hide_console_window() {
    unsafe {
        let console = FindWindowW(std::ptr::null(), std::ptr::null());
        if !console.is_null() {
            windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(console, SW_HIDE);
        }
    }
}

#[cfg(windows)]
fn pump_windows_messages() {
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE) != 0 {
            if msg.message == WM_QUIT {
                std::process::exit(0);
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
