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
use std::cell::RefCell;

#[cfg(windows)]
use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, ShowWindow, SW_HIDE};

// ── 拖放通道（WndProc → 主循环）────────────────────────────────────────────
#[cfg(windows)]
static DROP_CHANNEL: std::sync::OnceLock<std::sync::Mutex<std::sync::mpsc::Sender<String>>> =
    std::sync::OnceLock::new();

// ── 支持的音频扩展名 ─────────────────────────────────────────────────────────
#[cfg(windows)]
const SUPPORTED_AUDIO_EXTS: &[&str] = &[
    // 常见有损
    "mp3", "mp2", "mp1", "aac", "m4a", "m4b", "ogg", "opus", "wma", // 无损 / PCM
    "wav", "wave", "flac", "aiff", "aif", "aifc", // 容器
    "mp4", "m4v", "mkv", "mka", "webm", // 其他
    "adpcm", "caf", "au", "snd", "ape", "wv",
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
            "auto-voice  |  按住 [{}] 讲话，松开后自动粘贴  |  拖入音频文件即可转录",
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
        Graphics::Gdi::{CreateRoundRectRgn, SetWindowRgn},
        System::LibraryLoader::GetModuleHandleW,
        UI::HiDpi::{GetDpiForSystem, SetProcessDpiAwareness, PROCESS_PER_MONITOR_DPI_AWARE},
        UI::Shell::DragAcceptFiles,
        UI::WindowsAndMessaging::{
            CreateWindowExW, GetSystemMetrics, RegisterClassExW, WNDCLASSEXW,
        },
    };

    // 设置每显示器 DPI 感知
    unsafe { SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE); }

    // 窗口样式常量（inline 避免 feature 不确定导致的编译错误）
    const WS_POPUP: u32 = 0x8000_0000;
    const WS_VISIBLE: u32 = 0x1000_0000;
    const WS_EX_TOPMOST: u32 = 0x0000_0008;
    const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
    const WS_EX_NOACTIVATE: u32 = 0x0800_0000;
    const WS_EX_ACCEPTFILES: u32 = 0x0000_0010;
    const SM_CXSCREEN: i32 = 0;
    const SM_CYSCREEN: i32 = 1;

    // 基础尺寸（96 DPI 时的尺寸，BASE 是 DIP）
    const BASE_WIN_W: i32 = 320;
    const BASE_WIN_H: i32 = 66;

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

        // 使用系统 DPI 计算窗口物理尺寸
        let system_dpi = GetDpiForSystem().max(96) as f32;
        let scale = system_dpi / 96.0;
        let win_w = (BASE_WIN_W as f32 * scale) as i32;
        let win_h = (BASE_WIN_H as f32 * scale) as i32;

        tracing::info!("[DROPZONE] System DPI: {}, scale: {}, window: {}x{}",
            system_dpi as u32, scale, win_w, win_h);

        // 定位到屏幕右下角（避开任务栏约 50px）
        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        let screen_h = GetSystemMetrics(SM_CYSCREEN);
        let x = screen_w - win_w - (16.0 * scale) as i32;
        let y = screen_h - win_h - (56.0 * scale) as i32;

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_ACCEPTFILES,
            class_name.as_ptr(),
            std::ptr::null(), // 无标题栏文字
            WS_POPUP | WS_VISIBLE,
            x,
            y,
            win_w,
            win_h,
            std::ptr::null_mut(), // 桌面为父
            std::ptr::null_mut(), // 无菜单
            hinstance,
            std::ptr::null(),
        );

        if !hwnd.is_null() {
            let rgn = CreateRoundRectRgn(0, 0, win_w + 1, win_h + 1, win_h, win_h);
            SetWindowRgn(hwnd, rgn, 1);
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
    use windows_sys::Win32::UI::Shell::{DragFinish, DragQueryFileW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{DefWindowProcW, SendMessageW};

    // 消息常量
    const WM_LBUTTONDOWN: u32 = 0x0201;
    const WM_NCLBUTTONDOWN: u32 = 0x00A1;
    const WM_PAINT: u32 = 0x000F;
    const WM_ERASEBKGND: u32 = 0x0014;
    const WM_CLOSE: u32 = 0x0010;
    const WM_DROPFILES: u32 = 0x0233;
    const HTCAPTION: usize = 2;

    match msg {
        // 不擦除背景，由 WM_PAINT 全量绘制
        WM_ERASEBKGND => return 1,

        // 防止用户意外关闭浮窗
        WM_CLOSE => return 0,

        WM_LBUTTONDOWN => {
            SendMessageW(hwnd, WM_NCLBUTTONDOWN, HTCAPTION, lparam);
            return 0;
        }

        WM_PAINT => {
            draw_drop_zone(hwnd);
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

#[cfg(windows)]
thread_local! {
    static DROP_ZONE_RENDERER: RefCell<Option<DropZoneRenderer>> = const { RefCell::new(None) };
}

#[cfg(windows)]
struct DropZoneRenderer {
    target: windows::Win32::Graphics::Direct2D::ID2D1HwndRenderTarget,
    render_target: windows::Win32::Graphics::Direct2D::ID2D1RenderTarget,
    title_format: windows::Win32::Graphics::DirectWrite::IDWriteTextFormat,
    subtitle_format: windows::Win32::Graphics::DirectWrite::IDWriteTextFormat,
    caption_format: windows::Win32::Graphics::DirectWrite::IDWriteTextFormat,
    outer_brush: windows::Win32::Graphics::Direct2D::ID2D1SolidColorBrush,
    inner_brush: windows::Win32::Graphics::Direct2D::ID2D1SolidColorBrush,
    border_brush: windows::Win32::Graphics::Direct2D::ID2D1SolidColorBrush,
    accent_brush: windows::Win32::Graphics::Direct2D::ID2D1SolidColorBrush,
    accent_soft_brush: windows::Win32::Graphics::Direct2D::ID2D1SolidColorBrush,
    title_brush: windows::Win32::Graphics::Direct2D::ID2D1SolidColorBrush,
    subtitle_brush: windows::Win32::Graphics::Direct2D::ID2D1SolidColorBrush,
    caption_brush: windows::Win32::Graphics::Direct2D::ID2D1SolidColorBrush,
}

#[cfg(windows)]
fn draw_drop_zone(hwnd: windows_sys::Win32::Foundation::HWND) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::ValidateRect;

    let hwnd = HWND(hwnd as *mut std::ffi::c_void);
    let _ = with_drop_zone_renderer(hwnd, |renderer| unsafe {
        use windows::Win32::Graphics::Direct2D::Common::{D2D_POINT_2F, D2D_RECT_F};
        use windows::Win32::Graphics::Direct2D::{
            D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_ELLIPSE, D2D1_ROUNDED_RECT,
        };
        use windows::Win32::Graphics::DirectWrite::DWRITE_MEASURING_MODE_NATURAL;

        // DIP（设计像素）坐标，Direct2D 自动转换为物理像素
        let w = 320.0_f32;
        let h = 66.0_f32;
        let r = 33.0_f32;

        renderer.target.BeginDraw();
        renderer.render_target.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        renderer.render_target.Clear(Some(&color(0.0, 0.0, 0.0, 0.0)));

        // ── Layer 1: outer shell ──────────────────────────────────────────────
        renderer.render_target.FillRoundedRectangle(
            &D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F { left: 0.0, top: 0.0, right: w, bottom: h },
                radiusX: r, radiusY: r,
            },
            &renderer.outer_brush,
        );

        // ── Layer 2: inner shell ──────────────────────────────────────────────
        renderer.render_target.FillRoundedRectangle(
            &D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F { left: 1.5, top: 1.5, right: w - 1.5, bottom: h - 1.5 },
                radiusX: r - 1.5, radiusY: r - 1.5,
            },
            &renderer.inner_brush,
        );

        // ── Layer 3: top glaze highlight (like OSD) ───────────────────────────
        renderer.render_target.FillRoundedRectangle(
            &D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F { left: 8.0, top: 4.0, right: w - 8.0, bottom: h / 2.0 },
                radiusX: r - 10.0, radiusY: r - 10.0,
            },
            &renderer.caption_brush, // repurposed as glaze: (1,1,1,0.06)
        );

        // ── Layer 4: border stroke ────────────────────────────────────────────
        renderer.render_target.DrawRoundedRectangle(
            &D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F { left: 0.75, top: 0.75, right: w - 0.75, bottom: h - 0.75 },
                radiusX: r, radiusY: r,
            },
            &renderer.border_brush,
            1.0,
            None,
        );

        // ── Left icon lens — same glow stack as OSD status lens ───────────────
        let cx = 40.0_f32;
        let cy = 33.0_f32;

        renderer.render_target.FillEllipse(
            &D2D1_ELLIPSE { point: D2D_POINT_2F { x: cx, y: cy }, radiusX: 20.0, radiusY: 20.0 },
            &renderer.accent_soft_brush,
        );
        renderer.render_target.FillEllipse(
            &D2D1_ELLIPSE { point: D2D_POINT_2F { x: cx, y: cy }, radiusX: 13.0, radiusY: 13.0 },
            &renderer.accent_soft_brush,
        );
        renderer.render_target.DrawEllipse(
            &D2D1_ELLIPSE { point: D2D_POINT_2F { x: cx, y: cy }, radiusX: 10.0, radiusY: 10.0 },
            &renderer.accent_brush,
            1.5,
            None,
        );

        // Download-arrow icon (↓ + baseline) inside the lens
        renderer.render_target.DrawLine(
            D2D_POINT_2F { x: cx, y: cy - 7.0 },
            D2D_POINT_2F { x: cx, y: cy + 2.5 },
            &renderer.accent_brush, 2.0, None,
        );
        renderer.render_target.DrawLine(
            D2D_POINT_2F { x: cx - 4.5, y: cy - 1.5 },
            D2D_POINT_2F { x: cx, y: cy + 3.5 },
            &renderer.accent_brush, 2.0, None,
        );
        renderer.render_target.DrawLine(
            D2D_POINT_2F { x: cx + 4.5, y: cy - 1.5 },
            D2D_POINT_2F { x: cx, y: cy + 3.5 },
            &renderer.accent_brush, 2.0, None,
        );
        renderer.render_target.DrawLine(
            D2D_POINT_2F { x: cx - 6.0, y: cy + 7.0 },
            D2D_POINT_2F { x: cx + 6.0, y: cy + 7.0 },
            &renderer.accent_brush, 1.5, None,
        );

        // ── Text ──────────────────────────────────────────────────────────────
        let title: Vec<u16> = "拖入音频文件".encode_utf16().collect();
        let subtitle: Vec<u16> = "松手后自动开始转录".encode_utf16().collect();

        renderer.render_target.DrawText(
            &title,
            &renderer.title_format,
            &D2D_RECT_F { left: 70.0, top: 10.0, right: 300.0, bottom: 36.0 },
            &renderer.title_brush,
            Default::default(),
            DWRITE_MEASURING_MODE_NATURAL,
        );
        renderer.render_target.DrawText(
            &subtitle,
            &renderer.subtitle_format,
            &D2D_RECT_F { left: 70.0, top: 34.0, right: 300.0, bottom: 58.0 },
            &renderer.subtitle_brush,
            Default::default(),
            DWRITE_MEASURING_MODE_NATURAL,
        );

        let _ = renderer.target.EndDraw(None, None);
        Ok(())
    });

    unsafe {
        let _ = ValidateRect(hwnd, None);
    }
}

#[cfg(windows)]
fn with_drop_zone_renderer<F>(
    hwnd: windows::Win32::Foundation::HWND,
    f: F,
) -> windows::core::Result<()>
where
    F: FnOnce(&mut DropZoneRenderer) -> windows::core::Result<()>,
{
    DROP_ZONE_RENDERER.with(|slot| {
        if slot.borrow().is_none() {
            *slot.borrow_mut() = Some(create_drop_zone_renderer(hwnd)?);
        }
        let mut borrowed = slot.borrow_mut();
        let renderer = borrowed.as_mut().expect("drop zone renderer must exist");
        f(renderer)
    })
}

#[cfg(windows)]
fn create_drop_zone_renderer(
    hwnd: windows::Win32::Foundation::HWND,
) -> windows::core::Result<DropZoneRenderer> {
    use windows::core::{w, Interface};
    use windows::Win32::Graphics::Direct2D::Common::{
        D2D1_ALPHA_MODE_IGNORE, D2D1_PIXEL_FORMAT, D2D_SIZE_U,
    };
    use windows::Win32::Graphics::Direct2D::{
        D2D1CreateFactory, ID2D1Factory, ID2D1HwndRenderTarget, ID2D1RenderTarget,
        D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT,
        D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_PRESENT_OPTIONS_NONE,
        D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_DEFAULT,
    };
    use windows::Win32::Graphics::DirectWrite::{
        DWriteCreateFactory, IDWriteFactory, DWRITE_FACTORY_TYPE_SHARED,
        DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_MEDIUM,
        DWRITE_FONT_WEIGHT_NORMAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
        DWRITE_TEXT_ALIGNMENT_LEADING,
    };
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

    // 获取窗口实际 DPI 和尺寸
    let dpi = unsafe { GetDpiForWindow(hwnd).max(96) };

    let mut rect = windows::Win32::Foundation::RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect)? };
    let win_w = (rect.right - rect.left) as u32;
    let win_h = (rect.bottom - rect.top) as u32;

    tracing::info!("[DROPZONE] Renderer: dpi={}, window={}x{}", dpi, win_w, win_h);

    let factory: ID2D1Factory =
        unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)? };
    let dwrite_factory: IDWriteFactory =
        unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };

    let render_props = D2D1_RENDER_TARGET_PROPERTIES {
        r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_UNKNOWN,
            alphaMode: D2D1_ALPHA_MODE_IGNORE,
        },
        dpiX: dpi as f32,
        dpiY: dpi as f32,
        usage: Default::default(),
        minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
    };
    let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
        hwnd,
        pixelSize: D2D_SIZE_U {
            width: win_w,
            height: win_h,
        },
        presentOptions: D2D1_PRESENT_OPTIONS_NONE,
    };
    let target: ID2D1HwndRenderTarget =
        unsafe { factory.CreateHwndRenderTarget(&render_props, &hwnd_props)? };
    let render_target: ID2D1RenderTarget = target.cast()?;

    let title_format = unsafe {
        dwrite_factory.CreateTextFormat(
            w!("Segoe UI Variable"),
            None,
            DWRITE_FONT_WEIGHT_MEDIUM,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            15.0, // DIP，DirectWrite 自动按 render target DPI 缩放
            w!(""),
        )?
    };
    unsafe {
        title_format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
        title_format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
    }

    let subtitle_format = unsafe {
        dwrite_factory.CreateTextFormat(
            w!("Segoe UI Variable"),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            11.0, // DIP
            w!(""),
        )?
    };
    unsafe {
        subtitle_format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
        subtitle_format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
    }

    let caption_format = unsafe {
        dwrite_factory.CreateTextFormat(
            w!("Segoe UI Variable"),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            10.0, // DIP
            w!(""),
        )?
    };
    unsafe {
        caption_format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
        caption_format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
    }

    unsafe {
        Ok(DropZoneRenderer {
            target,
            render_target: render_target.clone(),
            title_format,
            subtitle_format,
            caption_format,
            outer_brush: render_target
                .CreateSolidColorBrush(&color(0.10, 0.13, 0.16, 1.0), None)?,
            inner_brush: render_target
                .CreateSolidColorBrush(&color(0.13, 0.17, 0.21, 1.0), None)?,
            border_brush: render_target
                .CreateSolidColorBrush(&color(0.74, 0.82, 0.90, 0.28), None)?,
            accent_brush: render_target
                .CreateSolidColorBrush(&color(0.54, 0.79, 1.0, 1.0), None)?,
            accent_soft_brush: render_target
                .CreateSolidColorBrush(&color(0.54, 0.79, 1.0, 0.18), None)?,
            title_brush: render_target
                .CreateSolidColorBrush(&color(0.96, 0.97, 0.98, 0.96), None)?,
            subtitle_brush: render_target
                .CreateSolidColorBrush(&color(0.86, 0.91, 0.95, 0.86), None)?,
            caption_brush: render_target
                .CreateSolidColorBrush(&color(1.0, 1.0, 1.0, 0.06), None)?, // top glaze
        })
    }
}

#[cfg(windows)]
fn color(
    r: f32,
    g: f32,
    b: f32,
    a: f32,
) -> windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F {
    windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F { r, g, b, a }
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
