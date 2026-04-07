/// 系统托盘模式：双击 exe 无参数启动时进入。
///
/// - 托盘图标常驻通知区域
/// - 自动进入 PTT 模式（配置键触发录音）
/// - 右下角浮动窗口：拖入音频文件 → 自动打开终端转录（含说话人分离）
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
use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, ShowWindow, SW_HIDE};

// ── 拖放通道（WndProc → 主循环）────────────────────────────────────────────
#[cfg(windows)]
static DROP_CHANNEL: std::sync::OnceLock<
    std::sync::Mutex<std::sync::mpsc::Sender<String>>,
> = std::sync::OnceLock::new();

// ── 支持的音频扩展名 ─────────────────────────────────────────────────────────
#[cfg(windows)]
const SUPPORTED_AUDIO_EXTS: &[&str] = &[
    // 常见有损
    "mp3", "mp2", "mp1",
    "aac", "m4a", "m4b",
    "ogg", "opus",
    "wma",
    // 无损 / PCM
    "wav", "wave",
    "flac",
    "aiff", "aif", "aifc",
    // 容器
    "mp4", "m4v",
    "mkv", "mka",
    "webm",
    // 其他
    "adpcm",
    "caf", "au", "snd",
    "ape", "wv",
];

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
            "auto-voice  |  按住 [{}] 录音  |  拖入音频文件到右下角浮窗转录",
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

    // ── 初始化拖放浮窗 ───────────────────────────────────────────────────────
    #[cfg(windows)]
    let drop_rx = setup_drop_zone()?;

    // ── 主线程：消息泵 + 托盘事件循环 ───────────────────────────────────────
    let quit_id = quit_item.id().clone();

    loop {
        // 右键菜单事件
        if let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == quit_id {
                std::process::exit(0);
            }
        }

        // 处理拖放进来的文件
        #[cfg(windows)]
        while let Ok(path) = drop_rx.try_recv() {
            tracing::info!("拖入文件: {}", path);
            if is_supported_audio(&path) {
                launch_transcribe_terminal(&path);
            } else {
                tracing::warn!("不支持的音频格式，已忽略: {}", path);
            }
        }

        // Windows 消息泵（托盘 + 拖放窗口）
        #[cfg(windows)]
        pump_windows_messages();

        std::thread::sleep(Duration::from_millis(16)); // ~60 fps 响应
    }
}

// ── Windows 拖放实现 ─────────────────────────────────────────────────────────

#[cfg(windows)]
fn setup_drop_zone() -> Result<std::sync::mpsc::Receiver<String>> {
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    DROP_CHANNEL
        .set(std::sync::Mutex::new(tx))
        .map_err(|_| anyhow::anyhow!("Drop channel already initialized"))?;
    create_drop_zone_window();
    Ok(rx)
}

#[cfg(windows)]
fn is_supported_audio(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    SUPPORTED_AUDIO_EXTS
        .iter()
        .any(|ext| lower.ends_with(&format!(".{}", ext)))
}

/// 打开一个新的 cmd 终端窗口，运行 `auto-voice transcribe --diarize "<path>"`
#[cfg(windows)]
fn launch_transcribe_terminal(file_path: &str) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("无法获取可执行文件路径: {}", e);
            return;
        }
    };

    // 构造命令行：用 /k 让窗口在转录完成后保持打开，方便查看结果
    let cmd = format!(
        "\"{}\" transcribe --diarize \"{}\"",
        exe.to_string_lossy(),
        file_path.replace('"', "\\\""),
    );

    match std::process::Command::new("cmd.exe")
        .args(["/k", &cmd])
        .creation_flags(CREATE_NEW_CONSOLE)
        .spawn()
    {
        Ok(_) => tracing::info!("已启动转录终端: {}", file_path),
        Err(e) => tracing::error!("启动终端失败: {}", e),
    }
}

/// 创建右下角浮动拖放接收窗口（始终置顶，接受文件拖入）
#[cfg(windows)]
fn create_drop_zone_window() {
    use windows_sys::Win32::{
        System::LibraryLoader::GetModuleHandleW,
        UI::Shell::DragAcceptFiles,
        UI::WindowsAndMessaging::{
            CreateWindowExW, GetSystemMetrics, RegisterClassExW,
            SetLayeredWindowAttributes, WNDCLASSEXW,
        },
    };

    // 窗口样式常量（inline 避免 feature 不确定导致的编译错误）
    const WS_POPUP: u32 = 0x8000_0000;
    const WS_VISIBLE: u32 = 0x1000_0000;
    const WS_EX_TOPMOST: u32 = 0x0000_0008;
    const WS_EX_LAYERED: u32 = 0x0008_0000;
    const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
    const WS_EX_NOACTIVATE: u32 = 0x0800_0000;
    const WS_EX_ACCEPTFILES: u32 = 0x0000_0010;
    const LWA_ALPHA: u32 = 0x0000_0002;
    const SM_CXSCREEN: i32 = 0;
    const SM_CYSCREEN: i32 = 1;

    const WIN_W: i32 = 264;
    const WIN_H: i32 = 96;

    unsafe {
        let class_name: Vec<u16> = "AutoVoiceDropZone"
            .encode_utf16()
            .chain(std::iter::once(0u16))
            .collect();

        let hinstance = GetModuleHandleW(std::ptr::null());

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(drop_zone_wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(), // 由 WM_PAINT 自绘
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        RegisterClassExW(&wc);

        // 定位到屏幕右下角（避开任务栏约 50px）
        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        let screen_h = GetSystemMetrics(SM_CYSCREEN);
        let x = screen_w - WIN_W - 16;
        let y = screen_h - WIN_H - 56;

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_ACCEPTFILES,
            class_name.as_ptr(),
            std::ptr::null(), // 无标题栏文字
            WS_POPUP | WS_VISIBLE,
            x,
            y,
            WIN_W,
            WIN_H,
            std::ptr::null_mut(), // 桌面为父
            std::ptr::null_mut(), // 无菜单
            hinstance,
            std::ptr::null(),
        );

        if !hwnd.is_null() {
            // 半透明：约 85% 不透明
            SetLayeredWindowAttributes(hwnd, 0, 218, LWA_ALPHA);
            // 接受文件拖入
            DragAcceptFiles(hwnd, 1 /* TRUE */);
        }
    }
}

/// 拖放窗口的 WndProc：绘制界面 + 处理 WM_DROPFILES
#[cfg(windows)]
unsafe extern "system" fn drop_zone_wnd_proc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::{
        Graphics::Gdi::{
            BeginPaint, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect,
            GetStockObject, SelectObject, SetBkMode, SetTextColor, PAINTSTRUCT,
        },
        UI::Shell::{DragFinish, DragQueryFileW},
        UI::WindowsAndMessaging::{DefWindowProcW, GetClientRect},
    };

    // 消息常量
    const WM_PAINT: u32 = 0x000F;
    const WM_ERASEBKGND: u32 = 0x0014;
    const WM_CLOSE: u32 = 0x0010;
    const WM_DROPFILES: u32 = 0x0233;

    // GDI 常量
    const TRANSPARENT_BK: i32 = 1; // TRANSPARENT mode for SetBkMode
    const DT_CENTER: u32 = 0x0000_0001;
    const DT_WORDBREAK: u32 = 0x0000_0010;
    const DEFAULT_GUI_FONT: i32 = 17;

    match msg {
        // 不擦除背景，由 WM_PAINT 全量绘制
        WM_ERASEBKGND => return 1,

        // 防止用户意外关闭浮窗
        WM_CLOSE => return 0,

        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);

            let mut rc: windows_sys::Win32::Foundation::RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rc);

            // ── 背景：深色圆角感（深蓝灰 #1e2a38）──────────────────────────
            // COLORREF 格式为 0x00BBGGRR
            let bg_brush = CreateSolidBrush(0x00_38_2a_1e);
            FillRect(hdc, &rc, bg_brush);
            DeleteObject(bg_brush);

            // ── 内边框高亮（略浅色 #3a4a5a）─────────────────────────────────
            let border_brush = CreateSolidBrush(0x00_5a_4a_3a);
            let border_rc = windows_sys::Win32::Foundation::RECT {
                left: rc.left + 1,
                top: rc.top + 1,
                right: rc.right - 1,
                bottom: rc.bottom - 1,
            };
            // 仅绘制 1px 边框（用 FillRect 绘制 4 条细线）
            let top_line = windows_sys::Win32::Foundation::RECT {
                left: border_rc.left,
                top: border_rc.top,
                right: border_rc.right,
                bottom: border_rc.top + 1,
            };
            let bottom_line = windows_sys::Win32::Foundation::RECT {
                left: border_rc.left,
                top: border_rc.bottom - 1,
                right: border_rc.right,
                bottom: border_rc.bottom,
            };
            let left_line = windows_sys::Win32::Foundation::RECT {
                left: border_rc.left,
                top: border_rc.top,
                right: border_rc.left + 1,
                bottom: border_rc.bottom,
            };
            let right_line = windows_sys::Win32::Foundation::RECT {
                left: border_rc.right - 1,
                top: border_rc.top,
                right: border_rc.right,
                bottom: border_rc.bottom,
            };
            FillRect(hdc, &top_line, border_brush);
            FillRect(hdc, &bottom_line, border_brush);
            FillRect(hdc, &left_line, border_brush);
            FillRect(hdc, &right_line, border_brush);
            DeleteObject(border_brush);

            // ── 文字：白色，居中，两行 ────────────────────────────────────────
            SetBkMode(hdc, TRANSPARENT_BK);
            SetTextColor(hdc, 0x00_FF_FF_FF); // 白色

            let font = GetStockObject(DEFAULT_GUI_FONT);
            let old_font = SelectObject(hdc, font);

            let text: Vec<u16> = "拖入音频文件\n自动转录 + 说话人分离"
                .encode_utf16()
                .chain(std::iter::once(0u16))
                .collect();

            let mut text_rc = windows_sys::Win32::Foundation::RECT {
                left: rc.left + 8,
                top: rc.top + 18,
                right: rc.right - 8,
                bottom: rc.bottom - 8,
            };

            DrawTextW(hdc, text.as_ptr(), -1, &mut text_rc, DT_CENTER | DT_WORDBREAK);

            SelectObject(hdc, old_font);
            EndPaint(hwnd, &ps);
            return 0;
        }

        WM_DROPFILES => {
            let hdrop = wparam as *mut std::ffi::c_void;
            // iFile = 0xFFFFFFFF → 返回文件数量
            let count = DragQueryFileW(hdrop, 0xFFFF_FFFFu32, std::ptr::null_mut(), 0);
            for i in 0..count {
                let len = DragQueryFileW(hdrop, i, std::ptr::null_mut(), 0) as usize;
                if len > 0 {
                    let mut buf = vec![0u16; len + 1];
                    DragQueryFileW(hdrop, i, buf.as_mut_ptr(), (len + 1) as u32);
                    let path = String::from_utf16_lossy(&buf[..len]).to_string();
                    if let Some(lock) = DROP_CHANNEL.get() {
                        if let Ok(sender) = lock.lock() {
                            let _ = sender.send(path);
                        }
                    }
                }
            }
            DragFinish(hdrop);
            return 0;
        }

        _ => {}
    }

    DefWindowProcW(hwnd, msg, wparam, lparam)
}

// ── 图标、消息泵、隐藏控制台（原有实现）───────────────────────────────────────

/// 生成 32×32 RGBA 图标：深色背景 + 红色圆点（表示录音状态）
fn build_icon() -> tray_icon::Icon {
    const SIZE: u32 = 32;
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];

    for y in 0..SIZE {
        for x in 0..SIZE {
            let cx = x as f32 - SIZE as f32 / 2.0 + 0.5;
            let cy = y as f32 - SIZE as f32 / 2.0 + 0.5;
            let dist = (cx * cx + cy * cy).sqrt();
            let idx = ((y * SIZE + x) * 4) as usize;

            if dist < 13.0 {
                rgba[idx] = 50;
                rgba[idx + 1] = 50;
                rgba[idx + 2] = 50;
                rgba[idx + 3] = 255;
            }
            if dist < 9.0 {
                rgba[idx] = 220;
                rgba[idx + 1] = 50;
                rgba[idx + 2] = 50;
                rgba[idx + 3] = 255;
            }
            if dist < 4.0 {
                rgba[idx] = 255;
                rgba[idx + 1] = 120;
                rgba[idx + 2] = 120;
                rgba[idx + 3] = 255;
            }
        }
    }

    tray_icon::Icon::from_rgba(rgba, SIZE, SIZE).expect("Failed to create tray icon")
}

#[cfg(windows)]
fn pump_windows_messages() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(windows)]
fn hide_console_window() {
    unsafe {
        let console = FindWindowW(std::ptr::null(), std::ptr::null());
        if !console.is_null() {
            ShowWindow(console, SW_HIDE);
        }
    }
}
