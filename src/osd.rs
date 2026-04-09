/// Win32 OSD — 浮于所有窗口上方的状态提示。
///
/// 这一版改成真正的胶囊外轮廓窗口，并把视觉拆成多层玻璃结构：
/// - 外壳 / 内胆 / 顶部釉面 / 底部反射
/// - 左侧状态透镜 + 右侧波形轨道
/// - 保留轻量状态机：Hidden → Recording → Processing → Done(1.8s) → Hidden

#[cfg(windows)]
pub use windows_impl::*;

#[cfg(not(windows))]
pub use stub_impl::*;

#[cfg(windows)]
mod windows_impl {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;

    use image::ImageFormat;
    use windows::core::{w, Interface, Result as WinResult, PCWSTR};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Direct2D::Common::{
        D2D1_ALPHA_MODE_IGNORE, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
        D2D_POINT_2F, D2D_RECT_F, D2D_SIZE_U,
    };
    use windows::Win32::Graphics::Direct2D::{
        D2D1CreateFactory, ID2D1Bitmap, ID2D1Factory, ID2D1HwndRenderTarget, ID2D1RenderTarget,
        ID2D1SolidColorBrush, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, D2D1_BITMAP_PROPERTIES, D2D1_ELLIPSE,
        D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT,
        D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_PRESENT_OPTIONS_NONE,
        D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_ROUNDED_RECT,
    };
    use windows::Win32::Graphics::DirectWrite::{
        DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
        DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_MEDIUM,
        DWRITE_FONT_WEIGHT_NORMAL, DWRITE_MEASURING_MODE_NATURAL,
        DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
        DWRITE_TEXT_ALIGNMENT_LEADING,
    };
    use windows::Win32::Graphics::Gdi::{
        CreateRoundRectRgn, InvalidateRect, SetWindowRgn, ValidateRect,
    };
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow, SetProcessDpiAwareness, PROCESS_PER_MONITOR_DPI_AWARE};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW,
        GetSystemMetrics, KillTimer, PostMessageW, RegisterClassExW, SetTimer,
        SetWindowLongPtrW, SetWindowPos, ShowWindow, CS_HREDRAW, CS_VREDRAW,
        GWLP_USERDATA, HWND_TOPMOST, MSG, SM_CXSCREEN, SM_CYSCREEN,
        SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE, SW_SHOWNOACTIVATE,
        WM_DESTROY, WM_NCDESTROY, WM_PAINT, WM_TIMER, WNDCLASSEXW, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    };

    const BASE_PLATE_TGA: &[u8] = include_bytes!("../assets/osd_pill_base.tga");

    // 绘制坐标系基于原始设计尺寸（缩小一半）
    const BASE_WIN_W: i32 = 180;
    const BASE_WIN_H: i32 = 40;

    fn dpi_scale_factor(dpi: u32) -> f32 {
        dpi as f32 / 96.0
    }

    // 获取系统 DPI（用于初始窗口创建）
    fn get_system_dpi() -> u32 {
        unsafe {
            GetDpiForSystem().max(96)
        }
    }

    fn get_window_dpi(hwnd: HWND) -> u32 {
        unsafe { GetDpiForWindow(hwnd).max(96) }
    }

    const STATE_HIDDEN: u32 = 0;
    const STATE_RECORDING: u32 = 1;
    const STATE_PROCESSING: u32 = 2;
    const STATE_DONE: u32 = 3;

    const WM_USER_STATE: u32 = 0x0401;
    const TIMER_DONE_HIDE: usize = 1;
    const TIMER_ANIM: usize = 2;

    static G_STATE: AtomicU32 = AtomicU32::new(STATE_HIDDEN);
    static G_FRAME: AtomicU32 = AtomicU32::new(0);
    static G_LEVEL: AtomicU32 = AtomicU32::new(0);
    static G_ELAPSED_MS: AtomicU32 = AtomicU32::new(0);
    static G_HWND: OnceLock<Mutex<isize>> = OnceLock::new();
    static G_RECORDING_START: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

    fn hwnd_cell() -> &'static Mutex<isize> {
        G_HWND.get_or_init(|| Mutex::new(0))
    }

    fn recording_start_cell() -> &'static Mutex<Option<Instant>> {
        G_RECORDING_START.get_or_init(|| Mutex::new(None))
    }

    fn current_hwnd() -> HWND {
        HWND(*hwnd_cell().lock().unwrap() as *mut std::ffi::c_void)
    }

    #[derive(Clone)]
    pub struct OsdHandle;

    impl OsdHandle {
        pub fn set_recording(&self) {
            post_state(STATE_RECORDING);
        }

        pub fn set_processing(&self) {
            post_state(STATE_PROCESSING);
        }

        pub fn set_done(&self) {
            post_state(STATE_DONE);
        }

        #[allow(dead_code)]
        pub fn hide(&self) {
            post_state(STATE_HIDDEN);
        }

        /// 检查是否可以开始新录音（在 PROCESSING/DONE 状态下返回 false）
        pub fn can_recording_start(&self) -> bool {
            let state = G_STATE.load(Ordering::SeqCst);
            state == STATE_HIDDEN
        }

        pub fn set_level(&self, level: f32) {
            let scaled = (level.clamp(0.0, 1.0) * 1000.0) as u32;
            G_LEVEL.store(scaled, Ordering::SeqCst);
            let hwnd = current_hwnd();
            if !hwnd.is_invalid() && G_STATE.load(Ordering::SeqCst) == STATE_RECORDING {
                unsafe {
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
        }
    }

    fn post_state(state: u32) {
        let now = Instant::now();
        match state {
            STATE_RECORDING => {
                *recording_start_cell().lock().unwrap() = Some(now);
                G_ELAPSED_MS.store(0, Ordering::SeqCst);
            }
            STATE_PROCESSING | STATE_DONE => {
                if let Some(start) = recording_start_cell().lock().unwrap().take() {
                    G_ELAPSED_MS.store(
                        now.duration_since(start).as_millis().min(u32::MAX as u128) as u32,
                        Ordering::SeqCst,
                    );
                }
            }
            STATE_HIDDEN => {
                *recording_start_cell().lock().unwrap() = None;
                G_ELAPSED_MS.store(0, Ordering::SeqCst);
            }
            _ => {}
        }
        G_STATE.store(state, Ordering::SeqCst);
        if state != STATE_RECORDING {
            G_LEVEL.store(0, Ordering::SeqCst);
        }
        let hwnd = current_hwnd();
        if !hwnd.is_invalid() {
            unsafe {
                let _ = PostMessageW(hwnd, WM_USER_STATE, WPARAM(0), LPARAM(0));
            }
        }
    }

    pub fn spawn_osd() -> OsdHandle {
        std::thread::spawn(run_osd_window);
        for _ in 0..50 {
            if *hwnd_cell().lock().unwrap() != 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        OsdHandle
    }

    #[derive(Clone)]
    struct Theme {
        shell_outer: D2D1_COLOR_F,
        shell_inner: D2D1_COLOR_F,
        shell_core: D2D1_COLOR_F,
        lens_outer: D2D1_COLOR_F,
        glaze: D2D1_COLOR_F,
        reflection: D2D1_COLOR_F,
        border_outer: D2D1_COLOR_F,
        border_inner: D2D1_COLOR_F,
        title: D2D1_COLOR_F,
        subtitle: D2D1_COLOR_F,
        accent: D2D1_COLOR_F,
        accent_soft: D2D1_COLOR_F,
        track: D2D1_COLOR_F,
        track_glow: D2D1_COLOR_F,
        button_face: D2D1_COLOR_F,
        button_border: D2D1_COLOR_F,
        danger_face: D2D1_COLOR_F,
    }

    struct Renderer {
        target: ID2D1HwndRenderTarget,
        render_target: ID2D1RenderTarget,
        baseplate: ID2D1Bitmap,
        title_format: IDWriteTextFormat,
        body_format: IDWriteTextFormat,
        title_brush: ID2D1SolidColorBrush,
        subtitle_brush: ID2D1SolidColorBrush,
        border_outer_brush: ID2D1SolidColorBrush,
        border_inner_brush: ID2D1SolidColorBrush,
        accent_brush: ID2D1SolidColorBrush,
        accent_soft_brush: ID2D1SolidColorBrush,
        track_brush: ID2D1SolidColorBrush,
        track_glow_brush: ID2D1SolidColorBrush,
        shell_outer_brush: ID2D1SolidColorBrush,
        shell_inner_brush: ID2D1SolidColorBrush,
        shell_core_brush: ID2D1SolidColorBrush,
        lens_outer_brush: ID2D1SolidColorBrush,
        glaze_brush: ID2D1SolidColorBrush,
        reflection_brush: ID2D1SolidColorBrush,
        button_face_brush: ID2D1SolidColorBrush,
        button_border_brush: ID2D1SolidColorBrush,
        danger_face_brush: ID2D1SolidColorBrush,
        waveform_brush: ID2D1SolidColorBrush,
        waveform_soft_brush: ID2D1SolidColorBrush,
        dpi_scale: f32,
    }

    thread_local! {
        static RENDERER: RefCell<Option<Renderer>> = const { RefCell::new(None) };
    }

    fn run_osd_window() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let _ = SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE);
            let hinstance = GetModuleHandleW(None).unwrap_or_default();

            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinstance.into(),
                hIcon: Default::default(),
                hCursor: Default::default(),
                hbrBackground: Default::default(),
                lpszMenuName: PCWSTR::null(),
                lpszClassName: w!("AutoVoiceOSD"),
                hIconSm: Default::default(),
            };
            let _ = RegisterClassExW(&wc);

            // 获取系统 DPI 用于初始窗口创建
            let system_dpi = get_system_dpi();
            let scale = dpi_scale_factor(system_dpi);
            tracing::info!("[OSD DPI] System DPI: {}, BASE: {}x{}, scale: {}",
                system_dpi, BASE_WIN_W, BASE_WIN_H, scale);

            // DPI 缩放后的窗口尺寸
            let win_w = (BASE_WIN_W as f32 * scale) as i32;
            let win_h = (BASE_WIN_H as f32 * scale) as i32;
            tracing::info!("[OSD DPI] Window pixel size: {}x{}", win_w, win_h);

            if win_w <= 0 || win_h <= 0 {
                tracing::error!("Invalid OSD window size: {}x{}", win_w, win_h);
                return;
            }

            let screen_w = GetSystemMetrics(SM_CXSCREEN);
            let screen_h = GetSystemMetrics(SM_CYSCREEN);
            let x = (screen_w - win_w) / 2;
            let y = (screen_h - win_h - (100.0 * scale) as i32).max((32.0 * scale) as i32);

            let hwnd = match CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("AutoVoiceOSD"),
                w!("auto-voice"),
                WS_POPUP,
                x, y, win_w, win_h,
                None, None, hinstance, None,
            ) {
                Ok(hwnd) => hwnd,
                Err(e) => {
                    tracing::error!("Failed to create OSD window: {:?}", e);
                    return;
                }
            };

            tracing::info!("OSD window created: {:?}", hwnd);

            // 验证窗口实际 DPI
            let window_dpi = get_window_dpi(hwnd);
            tracing::info!("[OSD DPI] Window actual DPI: {}, scale: {}",
                window_dpi, dpi_scale_factor(window_dpi));

            let rgn = CreateRoundRectRgn(0, 0, win_w + 1, win_h + 1, win_h / 2, win_h / 2);
            let _ = SetWindowRgn(hwnd, rgn, true);
            *hwnd_cell().lock().unwrap() = hwnd.0 as isize;

            // 初始隐藏窗口，由 WM_USER_STATE (STATE_HIDDEN) 处理
            let _ = ShowWindow(hwnd, SW_HIDE);
            tracing::info!("OSD window hidden initially, entering message loop");

            // 发送初始状态让消息循环处理
            let _ = PostMessageW(hwnd, WM_USER_STATE, WPARAM(0), LPARAM(0));

            let mut msg = MSG::default();
            let mut count = 0;
            loop {
                let ret = GetMessageW(&mut msg, None, 0, 0);
                tracing::info!("GetMessageW returned: {}", ret.0);
                if ret.0 < 0 {
                    tracing::error!("GetMessageW returned error: {}", ret.0);
                    break;
                }
                if ret.0 == 0 {
                    tracing::info!("Got WM_QUIT, exiting loop");
                    break;
                }
                count += 1;
                if count <= 10 {
                    tracing::info!("OSD msg: {:04x}", msg.message);
                }
                DispatchMessageW(&msg);
            }
            tracing::info!("OSD message loop exited after {} messages", count);
            *hwnd_cell().lock().unwrap() = 0;
        }
    }

    unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        match msg {
            WM_USER_STATE => {
                let state = G_STATE.load(Ordering::SeqCst);
                tracing::info!("WM_USER_STATE: state={}", state);
                if state == STATE_HIDDEN {
                    let _ = KillTimer(hwnd, TIMER_DONE_HIDE);
                    let _ = KillTimer(hwnd, TIMER_ANIM);
                    let _ = ShowWindow(hwnd, SW_HIDE);
                } else {
                    let _ = SetWindowPos(
                        hwnd,
                        HWND_TOPMOST,
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
                    );
                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

                    if state == STATE_RECORDING || state == STATE_PROCESSING {
                        let _ = KillTimer(hwnd, TIMER_DONE_HIDE);
                        let _ = SetTimer(hwnd, TIMER_ANIM, 33, None);
                    } else {
                        let _ = KillTimer(hwnd, TIMER_ANIM);
                        let _ = KillTimer(hwnd, TIMER_DONE_HIDE);
                        let _ = SetTimer(hwnd, TIMER_DONE_HIDE, 1800, None);
                    }
                    let _ = InvalidateRect(hwnd, None, false);
                }
                LRESULT(0)
            }
            WM_TIMER => {
                if wp.0 == TIMER_ANIM {
                    G_FRAME.fetch_add(1, Ordering::SeqCst);
                    let _ = InvalidateRect(hwnd, None, false);
                } else if wp.0 == TIMER_DONE_HIDE {
                    let _ = KillTimer(hwnd, TIMER_DONE_HIDE);
                    G_STATE.store(STATE_HIDDEN, Ordering::SeqCst);
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                LRESULT(0)
            }
            WM_PAINT => {
                tracing::info!("WM_PAINT received");
                draw(hwnd);
                tracing::info!("WM_PAINT handled");
                LRESULT(0)
            }
            WM_NCDESTROY => {
                RENDERER.with(|slot| {
                    slot.borrow_mut().take();
                });
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_DESTROY => {
                windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }

    unsafe fn draw(hwnd: HWND) {
        let result = with_renderer(hwnd, |renderer| {
            let state = G_STATE.load(Ordering::SeqCst);
            let frame = G_FRAME.load(Ordering::SeqCst);
            let level = G_LEVEL.load(Ordering::SeqCst) as f32 / 1000.0;
            let elapsed_ms = current_elapsed_ms(state);
            let theme = theme_for_state(state);
            let scale = renderer.dpi_scale;

            tracing::info!("OSD draw: state={}, scale={}, baseplate={:?}", state, scale, renderer.baseplate);

            renderer.target.BeginDraw();
            renderer
                .render_target
                .SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            renderer.render_target.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));

            renderer.shell_outer_brush.SetColor(&theme.shell_outer);
            renderer.shell_inner_brush.SetColor(&theme.shell_inner);
            renderer.shell_core_brush.SetColor(&theme.shell_core);
            renderer.lens_outer_brush.SetColor(&theme.lens_outer);
            renderer.glaze_brush.SetColor(&theme.glaze);
            renderer.reflection_brush.SetColor(&theme.reflection);
            renderer.border_outer_brush.SetColor(&theme.border_outer);
            renderer.border_inner_brush.SetColor(&theme.border_inner);
            renderer.title_brush.SetColor(&theme.title);
            renderer.subtitle_brush.SetColor(&theme.subtitle);
            renderer.accent_brush.SetColor(&theme.accent);
            renderer.accent_soft_brush.SetColor(&theme.accent_soft);
            renderer.track_brush.SetColor(&theme.track);
            renderer.track_glow_brush.SetColor(&theme.track_glow);
            renderer.button_face_brush.SetColor(&theme.button_face);
            renderer.button_border_brush.SetColor(&theme.button_border);
            renderer.danger_face_brush.SetColor(&theme.danger_face);

            // 渲染目标坐标是 DIP（设备无关像素）
            // pixelSize=540x120, dpiX/dpiY=144 意味着 DIP 尺寸 = 540/1.5 x 120/1.5 = 360x80
            // 所以绘制坐标不应超过 DIP 范围: 0-360 宽, 0-80 高
            // 但 baseplate 是 300x72 位图，绘制到 360x80 会变形
            // 为了填充整个窗口（360x80 DIP），需要非均匀缩放
            let outer = D2D_RECT_F {
                left: 0.0,
                top: 0.0,
                right: BASE_WIN_W as f32,  // 360 DIP
                bottom: BASE_WIN_H as f32, // 80 DIP
            };
            tracing::info!("[OSD DPI] DrawBitmap: base={}x{}, outer={}x{} DIP",
                BASE_WIN_W, BASE_WIN_H, outer.right, outer.bottom);
            renderer.render_target.DrawBitmap(
                &renderer.baseplate,
                Some(&outer),
                1.0,
                D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                None,
            );

            draw_status_lens(renderer, state, frame);
            draw_signal_line(renderer, state, frame, level);
            draw_timer(renderer, elapsed_ms);

            let _ = renderer.target.EndDraw(None, None);
            Ok(())
        });

        if let Err(e) = result {
            tracing::error!("OSD draw error: {:?}", e);
        }
        let _ = ValidateRect(hwnd, None);
    }

    unsafe fn with_renderer<F>(hwnd: HWND, f: F) -> WinResult<()>
    where
        F: FnOnce(&mut Renderer) -> WinResult<()>,
    {
        RENDERER.with(|slot| {
            if slot.borrow().is_none() {
                tracing::info!("Creating OSD renderer for hwnd {:?}", hwnd);
                match create_renderer(hwnd) {
                    Ok(r) => {
                        tracing::info!("OSD renderer created successfully");
                        *slot.borrow_mut() = Some(r);
                    }
                    Err(e) => {
                        tracing::error!("Failed to create OSD renderer: {:?}", e);
                        return Err(e);
                    }
                }
            }
            let mut borrowed = slot.borrow_mut();
            let renderer = borrowed.as_mut().expect("renderer must exist");
            f(renderer)
        })
    }

    unsafe fn create_renderer(hwnd: HWND) -> WinResult<Renderer> {
        let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let dwrite_factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;

        // 获取窗口实际 DPI（用于渲染缩放）
        let dpi = GetDpiForWindow(hwnd).max(96);
        let scale = dpi as f32 / 96.0;
        tracing::info!("[OSD DPI] Renderer: dpi={}, scale={}, base={}x{}",
            dpi, scale, BASE_WIN_W, BASE_WIN_H);

        // 计算缩放后的像素尺寸（用于窗口和渲染目标）
        let scaled_w = (BASE_WIN_W as f32 * scale) as u32;
        let scaled_h = (BASE_WIN_H as f32 * scale) as u32;
        tracing::info!("[OSD DPI] RenderTarget pixel size: {}x{}, dpi={}", scaled_w, scaled_h, dpi);

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
                width: scaled_w,
                height: scaled_h,
            },
            presentOptions: D2D1_PRESENT_OPTIONS_NONE,
        };
        let target = factory.CreateHwndRenderTarget(&render_props, &hwnd_props)?;
        tracing::info!("OSD CreateHwndRenderTarget succeeded: {}x{}", scaled_w, scaled_h);
        let render_target: ID2D1RenderTarget = target.cast()?;
        tracing::info!("OSD render_target cast succeeded");

        tracing::info!("Loading baseplate bitmap...");
        let baseplate = load_baseplate_bitmap(&render_target)?;
        tracing::info!("OSD baseplate loaded successfully");

        let title_format = dwrite_factory.CreateTextFormat(
            w!("Segoe UI Variable"),
            None,
            DWRITE_FONT_WEIGHT_MEDIUM,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            14.0, // 字体大小是 DIP，DirectWrite 会自动按 render target DPI 缩放
            w!(""),
        )?;
        title_format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
        title_format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR)?;

        let body_format = dwrite_factory.CreateTextFormat(
            w!("Segoe UI Variable"),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            10.5, // 字体大小是 DIP，DirectWrite 会自动按 render target DPI 缩放
            w!(""),
        )?;
        body_format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
        body_format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR)?;

        let title_brush =
            render_target.CreateSolidColorBrush(&rgb(0.95, 0.96, 0.98, 0.94), None)?;
        let subtitle_brush =
            render_target.CreateSolidColorBrush(&rgb(0.92, 0.94, 0.97, 0.60), None)?;
        let border_outer_brush =
            render_target.CreateSolidColorBrush(&rgb(1.0, 1.0, 1.0, 0.18), None)?;
        let border_inner_brush =
            render_target.CreateSolidColorBrush(&rgb(0.74, 0.84, 0.94, 0.16), None)?;
        let accent_brush =
            render_target.CreateSolidColorBrush(&rgb(0.95, 0.47, 0.35, 1.0), None)?;
        let accent_soft_brush =
            render_target.CreateSolidColorBrush(&rgb(0.95, 0.47, 0.35, 0.26), None)?;
        let track_brush =
            render_target.CreateSolidColorBrush(&rgb(0.14, 0.19, 0.25, 0.96), None)?;
        let track_glow_brush =
            render_target.CreateSolidColorBrush(&rgb(0.87, 0.93, 1.0, 0.10), None)?;
        let shell_outer_brush =
            render_target.CreateSolidColorBrush(&rgb(0.08, 0.10, 0.13, 1.0), None)?;
        let shell_inner_brush =
            render_target.CreateSolidColorBrush(&rgb(0.10, 0.13, 0.17, 1.0), None)?;
        let shell_core_brush =
            render_target.CreateSolidColorBrush(&rgb(0.12, 0.16, 0.20, 1.0), None)?;
        let lens_outer_brush =
            render_target.CreateSolidColorBrush(&rgb(0.16, 0.22, 0.29, 0.96), None)?;
        let glaze_brush = render_target.CreateSolidColorBrush(&rgb(1.0, 1.0, 1.0, 0.08), None)?;
        let reflection_brush =
            render_target.CreateSolidColorBrush(&rgb(0.70, 0.84, 0.96, 0.06), None)?;
        let button_face_brush =
            render_target.CreateSolidColorBrush(&rgb(0.29, 0.33, 0.36, 0.92), None)?;
        let button_border_brush =
            render_target.CreateSolidColorBrush(&rgb(1.0, 1.0, 1.0, 0.14), None)?;
        let danger_face_brush =
            render_target.CreateSolidColorBrush(&rgb(0.44, 0.16, 0.18, 0.94), None)?;
        // Fixed cyan-blue waveform color (matches reference design, independent of state accent)
        let waveform_brush =
            render_target.CreateSolidColorBrush(&rgb(0.20, 0.65, 0.90, 1.0), None)?;
        let waveform_soft_brush =
            render_target.CreateSolidColorBrush(&rgb(0.20, 0.65, 0.90, 0.22), None)?;

        Ok(Renderer {
            target,
            render_target,
            baseplate,
            title_format,
            body_format,
            title_brush,
            subtitle_brush,
            border_outer_brush,
            border_inner_brush,
            accent_brush,
            accent_soft_brush,
            track_brush,
            track_glow_brush,
            shell_outer_brush,
            shell_inner_brush,
            shell_core_brush,
            lens_outer_brush,
            glaze_brush,
            reflection_brush,
            button_face_brush,
            button_border_brush,
            danger_face_brush,
            waveform_brush,
            waveform_soft_brush,
            dpi_scale: scale,
        })
    }

    unsafe fn draw_status_lens(renderer: &Renderer, state: u32, frame: u32) {
        // 坐标是 DIP（设计像素），Direct2D 根据 render target DPI 自动转换为物理像素
        // 布局：填满整个 180×40 窗口（缩小一半）
        let center = D2D_POINT_2F { x: 20.0, y: 20.0 }; // 居中偏左
        let pulse = (frame as f32 * 0.12).sin() * 0.12 + 1.0;

        // Outer glow ring
        let outer_brush = match state {
            STATE_RECORDING => &renderer.accent_soft_brush,
            STATE_PROCESSING => &renderer.accent_soft_brush,
            STATE_DONE => &renderer.accent_soft_brush,
            _ => &renderer.track_glow_brush,
        };
        renderer.render_target.FillEllipse(
            &ellipse(center.x, center.y, 10.0 * pulse, 10.0 * pulse),
            outer_brush,
        );

        // Middle glow
        renderer.render_target.FillEllipse(
            &ellipse(center.x, center.y, 7.0 * pulse, 7.0 * pulse),
            &renderer.accent_soft_brush,
        );

        // Core ring
        renderer.render_target.DrawEllipse(
            &ellipse(center.x, center.y, 5.0, 5.0),
            &renderer.accent_brush,
            1.0,
            None,
        );

        // Inner fill
        renderer.render_target.FillEllipse(
            &ellipse(center.x, center.y, 4.0, 4.0),
            &renderer.accent_brush,
        );

        // Bright center dot
        renderer.render_target.FillEllipse(
            &ellipse(center.x, center.y, 2.0, 2.0),
            &renderer.title_brush,
        );
    }

    unsafe fn draw_rec_label(renderer: &Renderer, state: u32) {
        let label = match state {
            STATE_RECORDING => "REC",
            STATE_PROCESSING => "RUN",
            STATE_DONE => "DONE",
            _ => "LIVE",
        };

        // Choose brush color based on state
        let label_brush = match state {
            STATE_RECORDING => &renderer.accent_brush,
            STATE_PROCESSING => &renderer.accent_brush,
            STATE_DONE => &renderer.accent_brush,
            _ => &renderer.title_brush,
        };

        let label_wide: Vec<u16> = label.encode_utf16().collect();
        renderer.render_target.DrawText(
            &label_wide,
            &renderer.title_format,
            &D2D_RECT_F {
                left: 31.0,  // 紧跟在状态灯（x=20，半径10）后面
                top: 17.0,   // 与状态灯中心 y=20 对齐
                right: 54.0,
                bottom: 25.0,
            },
            label_brush,
            Default::default(),
            DWRITE_MEASURING_MODE_NATURAL,
        );
    }

    unsafe fn draw_signal_line(renderer: &Renderer, state: u32, frame: u32, level: f32) {
        // 坐标是 DIP，Direct2D 自动转换为物理像素
        // 布局填满 180×40：状态灯在左，波形在中间偏右
        const BARS: usize = 26;
        let left = 38.0_f32; // 状态灯右侧开始，稍向右移
        let right = 170.0_f32; // 窗口右边界
        let width = right - left;
        let baseline = 20.0_f32; // 垂直居中
        let bar_width = 1.5_f32;
        let gap = (width - BARS as f32 * bar_width) / (BARS as f32 - 1.0);
        let time = frame as f32 * 0.15;
        let max_bar_half = 8.0_f32; // 上下最大波动幅度

        for i in 0..BARS {
            let x = left + i as f32 * (bar_width + gap);
            let p = i as f32 / BARS as f32;
            let norm = match state {
                STATE_RECORDING => {
                    let env_main = gaussian(p, 0.30, 0.14) * 1.2;
                    let env_tail = gaussian(p, 0.60, 0.20) * 0.50;
                    let env_end  = gaussian(p, 0.85, 0.09) * 0.18;
                    let carrier  = (time + p * 12.0).sin() * 0.5 + 0.5;
                    let shaped   = (carrier * 0.6 + 0.4).clamp(0.0, 1.0);
                    ((env_main + env_tail + env_end) * shaped * level.powf(0.70)).clamp(0.0, 1.0)
                }
                STATE_PROCESSING => {
                    let env = gaussian(p, 0.35, 0.22) * 0.65
                            + gaussian(p, 0.65, 0.16) * 0.40;
                    let carrier = ((time * 1.2 + p * 8.0).sin() * 0.5 + 0.5) * 0.70 + 0.30;
                    (env * carrier).clamp(0.0, 0.80)
                }
                STATE_DONE => {
                    // Gentle fade-out shimmer — was nearly invisible at 0.45 norm ceiling
                    let env = gaussian(p, 0.45, 0.25) * 0.80;
                    let carrier = ((time * 0.8 + p * 5.5).sin() * 0.5 + 0.5) * 0.65 + 0.35;
                    (env * carrier).clamp(0.0, 0.75)
                }
                _ => 0.0,
            };

            // 上下对称波动：以 baseline 为中心，上下各 half_height
            let half_height = norm * max_bar_half;
            let top = baseline - half_height;
            let bottom = baseline + half_height;

            // Soft glow halo behind bar (上下扩展)
            let shadow_rect = D2D_RECT_F {
                left: x - 0.5,
                top: top - 1.0,
                right: x + bar_width + 0.5,
                bottom: bottom + 1.0,
            };
            renderer.render_target.FillRectangle(
                &shadow_rect,
                &renderer.accent_soft_brush,
            );

            // Main waveform bar — same accent color as status lens
            let bar_rect = D2D_RECT_F {
                left: x,
                top,
                right: x + bar_width,
                bottom,
            };
            renderer.render_target.FillRectangle(
                &bar_rect,
                &renderer.accent_brush,
            );
        }
    }

    unsafe fn draw_action_buttons(renderer: &Renderer, state: u32) {
        let right_x = 296.0;
        let top = 28.0;
        let size = 24.0;

        // Pause button background
        let pause_rect = D2D_RECT_F {
            left: right_x,
            top,
            right: right_x + size,
            bottom: top + size,
        };
        renderer.render_target.FillRoundedRectangle(
            &rounded_rect(pause_rect, 6.0),
            &renderer.button_face_brush,
        );
        renderer.render_target.DrawRoundedRectangle(
            &rounded_rect(pause_rect, 6.0),
            &renderer.button_border_brush,
            1.0,
            None,
        );

        // Close button background
        let close_rect = D2D_RECT_F {
            left: right_x + 30.0,
            top,
            right: right_x + 30.0 + size,
            bottom: top + size,
        };
        renderer.render_target.FillRoundedRectangle(
            &rounded_rect(close_rect, 6.0),
            &renderer.danger_face_brush,
        );
        renderer.render_target.DrawRoundedRectangle(
            &rounded_rect(close_rect, 6.0),
            &renderer.button_border_brush,
            1.0,
            None,
        );

        let icon_brush = if state == STATE_DONE {
            &renderer.title_brush
        } else {
            &renderer.subtitle_brush
        };

        // Pause icon (two vertical bars)
        renderer.render_target.DrawLine(
            D2D_POINT_2F {
                x: pause_rect.left + 8.0,
                y: pause_rect.top + 7.0,
            },
            D2D_POINT_2F {
                x: pause_rect.left + 8.0,
                y: pause_rect.bottom - 7.0,
            },
            icon_brush,
            2.0,
            None,
        );
        renderer.render_target.DrawLine(
            D2D_POINT_2F {
                x: pause_rect.right - 8.0,
                y: pause_rect.top + 7.0,
            },
            D2D_POINT_2F {
                x: pause_rect.right - 8.0,
                y: pause_rect.bottom - 7.0,
            },
            icon_brush,
            2.0,
            None,
        );

        // Close icon (X mark)
        renderer.render_target.DrawLine(
            D2D_POINT_2F {
                x: close_rect.left + 8.0,
                y: close_rect.top + 8.0,
            },
            D2D_POINT_2F {
                x: close_rect.right - 8.0,
                y: close_rect.bottom - 8.0,
            },
            &renderer.title_brush,
            2.0,
            None,
        );
        renderer.render_target.DrawLine(
            D2D_POINT_2F {
                x: close_rect.right - 8.0,
                y: close_rect.top + 8.0,
            },
            D2D_POINT_2F {
                x: close_rect.left + 8.0,
                y: close_rect.bottom - 8.0,
            },
            &renderer.title_brush,
            2.0,
            None,
        );
    }

    unsafe fn draw_timer(_renderer: &Renderer, _elapsed_ms: u32) {
        // 不再显示计时器文字
    }

    fn gaussian(x: f32, mean: f32, sigma: f32) -> f32 {
        let z = (x - mean) / sigma;
        (-0.5 * z * z).exp()
    }

    fn current_elapsed_ms(state: u32) -> u32 {
        if state == STATE_RECORDING {
            if let Some(start) = *recording_start_cell().lock().unwrap() {
                return Instant::now()
                    .duration_since(start)
                    .as_millis()
                    .min(u32::MAX as u128) as u32;
            }
        }
        G_ELAPSED_MS.load(Ordering::SeqCst)
    }

    fn format_mmss(ms: u32) -> String {
        let secs = ms / 1000;
        let m = secs / 60;
        let s = secs % 60;
        format!("{m:02}:{s:02}")
    }

    fn load_baseplate_bitmap(render_target: &ID2D1RenderTarget) -> WinResult<ID2D1Bitmap> {
        let dyn_img = image::load_from_memory_with_format(BASE_PLATE_TGA, ImageFormat::Tga)
            .map_err(|e| {
                windows::core::Error::new(
                    windows::core::HRESULT(0x80004005u32 as i32),
                    format!("failed to decode OSD baseplate: {e}"),
                )
            })?;
        let rgba = dyn_img.to_rgba8();
        let (width, height) = rgba.dimensions();
        let mut bytes = rgba.into_raw();
        for px in bytes.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        tracing::info!("[OSD DPI] Baseplate raw: {}x{}, BASE: {}x{}", width, height, BASE_WIN_W, BASE_WIN_H);

        // bitmap dpi 设为 96，表示图片设计分辨率是 96 DPI
        // Direct2D 会根据渲染目标 DPI 自动缩放
        let props = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
        };

        unsafe {
            render_target.CreateBitmap(
                D2D_SIZE_U { width, height },
                Some(bytes.as_ptr() as _),
                width * 4,
                &props,
            )
        }
    }

    fn theme_for_state(state: u32) -> Theme {
        match state {
            STATE_RECORDING => Theme {
                shell_outer: rgb(0.06, 0.08, 0.11, 1.0),
                shell_inner: rgb(0.10, 0.12, 0.16, 1.0),
                shell_core: rgb(0.13, 0.16, 0.20, 1.0),
                lens_outer: rgb(0.19, 0.18, 0.20, 1.0),
                glaze: rgb(1.0, 1.0, 1.0, 0.08),
                reflection: rgb(0.90, 0.54, 0.42, 0.06),
                border_outer: rgb(1.0, 1.0, 1.0, 0.18),
                border_inner: rgb(0.92, 0.54, 0.44, 0.16),
                title: rgb(0.96, 0.97, 0.98, 0.96),
                subtitle: rgb(0.92, 0.94, 0.97, 0.70),
                accent: rgb(0.96, 0.47, 0.35, 1.0),
                accent_soft: rgb(0.96, 0.47, 0.35, 0.20),
                track: rgb(0.17, 0.14, 0.15, 1.0),
                track_glow: rgb(0.96, 0.47, 0.35, 0.14),
                button_face: rgb(0.28, 0.32, 0.35, 0.96),
                button_border: rgb(1.0, 1.0, 1.0, 0.14),
                danger_face: rgb(0.46, 0.17, 0.18, 0.96),
            },
            STATE_PROCESSING => Theme {
                shell_outer: rgb(0.06, 0.08, 0.11, 1.0),
                shell_inner: rgb(0.10, 0.12, 0.16, 1.0),
                shell_core: rgb(0.13, 0.16, 0.20, 1.0),
                lens_outer: rgb(0.20, 0.19, 0.16, 1.0),
                glaze: rgb(1.0, 1.0, 1.0, 0.08),
                reflection: rgb(0.92, 0.78, 0.42, 0.06),
                border_outer: rgb(1.0, 1.0, 1.0, 0.18),
                border_inner: rgb(0.88, 0.76, 0.42, 0.16),
                title: rgb(0.96, 0.97, 0.98, 0.96),
                subtitle: rgb(0.92, 0.94, 0.97, 0.70),
                accent: rgb(0.85, 0.72, 0.36, 1.0),
                accent_soft: rgb(0.85, 0.72, 0.36, 0.20),
                track: rgb(0.19, 0.17, 0.13, 1.0),
                track_glow: rgb(0.85, 0.72, 0.36, 0.14),
                button_face: rgb(0.30, 0.32, 0.31, 0.96),
                button_border: rgb(1.0, 1.0, 1.0, 0.14),
                danger_face: rgb(0.46, 0.20, 0.18, 0.96),
            },
            STATE_DONE => Theme {
                shell_outer: rgb(0.06, 0.08, 0.11, 1.0),
                shell_inner: rgb(0.10, 0.12, 0.16, 1.0),
                shell_core: rgb(0.13, 0.16, 0.20, 1.0),
                lens_outer: rgb(0.15, 0.20, 0.18, 1.0),
                glaze: rgb(1.0, 1.0, 1.0, 0.08),
                reflection: rgb(0.56, 0.86, 0.74, 0.06),
                border_outer: rgb(1.0, 1.0, 1.0, 0.18),
                border_inner: rgb(0.50, 0.84, 0.70, 0.16),
                title: rgb(0.95, 0.97, 0.98, 0.96),
                subtitle: rgb(0.90, 0.94, 0.95, 0.70),
                accent: rgb(0.46, 0.84, 0.68, 1.0),
                accent_soft: rgb(0.46, 0.84, 0.68, 0.18),
                track: rgb(0.13, 0.18, 0.16, 1.0),
                track_glow: rgb(0.46, 0.84, 0.68, 0.14),
                button_face: rgb(0.25, 0.33, 0.30, 0.96),
                button_border: rgb(1.0, 1.0, 1.0, 0.14),
                danger_face: rgb(0.33, 0.18, 0.19, 0.96),
            },
            _ => Theme {
                shell_outer: rgb(0.06, 0.08, 0.11, 1.0),
                shell_inner: rgb(0.10, 0.12, 0.16, 1.0),
                shell_core: rgb(0.13, 0.16, 0.20, 1.0),
                lens_outer: rgb(0.16, 0.19, 0.23, 1.0),
                glaze: rgb(1.0, 1.0, 1.0, 0.08),
                reflection: rgb(0.64, 0.78, 0.96, 0.06),
                border_outer: rgb(1.0, 1.0, 1.0, 0.18),
                border_inner: rgb(0.66, 0.78, 0.94, 0.16),
                title: rgb(0.96, 0.97, 0.98, 0.96),
                subtitle: rgb(0.92, 0.94, 0.97, 0.70),
                accent: rgb(0.62, 0.72, 0.90, 1.0),
                accent_soft: rgb(0.62, 0.72, 0.90, 0.20),
                track: rgb(0.14, 0.18, 0.22, 1.0),
                track_glow: rgb(0.62, 0.72, 0.90, 0.14),
                button_face: rgb(0.28, 0.31, 0.35, 0.96),
                button_border: rgb(1.0, 1.0, 1.0, 0.14),
                danger_face: rgb(0.40, 0.18, 0.20, 0.96),
            },
        }
    }

    fn rgb(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
        D2D1_COLOR_F { r, g, b, a }
    }

    fn rounded_rect(rect: D2D_RECT_F, radius: f32) -> D2D1_ROUNDED_RECT {
        D2D1_ROUNDED_RECT {
            rect,
            radiusX: radius,
            radiusY: radius,
        }
    }

    fn ellipse(x: f32, y: f32, rx: f32, ry: f32) -> D2D1_ELLIPSE {
        D2D1_ELLIPSE {
            point: D2D_POINT_2F { x, y },
            radiusX: rx,
            radiusY: ry,
        }
    }
}

#[cfg(not(windows))]
mod stub_impl {
    #[derive(Clone)]
    pub struct OsdHandle;

    impl OsdHandle {
        pub fn set_recording(&self) {}
        pub fn set_processing(&self) {}
        pub fn set_done(&self) {}
        pub fn hide(&self) {}
        pub fn set_level(&self, _level: f32) {}
    }

    pub fn spawn_osd() -> OsdHandle {
        OsdHandle
    }
}
