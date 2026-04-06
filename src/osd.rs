/// Win32 Layered Window OSD — 浮于所有窗口上方的状态提示。
///
/// 状态机：Hidden → Recording → Processing → Done(2s) → Hidden
///
/// 视觉：
/// - 底部中间的圆角胶囊小窗
/// - 双缓冲绘制（无闪烁）
/// - 仅显示波形，不显示文字

#[cfg(windows)]
pub use windows_impl::*;

#[cfg(not(windows))]
pub use stub_impl::*;

#[cfg(windows)]
mod windows_impl {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Mutex, OnceLock};

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateRoundRectRgn,
        CreateSolidBrush, DeleteDC, DeleteObject, EndPaint, FillRect, GetStockObject,
        InvalidateRect, RoundRect, SelectObject, SetWindowRgn, HBRUSH, HDC, HGDIOBJ, PAINTSTRUCT,
        SRCCOPY,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::HiDpi::{SetProcessDpiAwareness, PROCESS_SYSTEM_DPI_AWARE};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetSystemMetrics,
        KillTimer, PostMessageW, RegisterClassExW, SetLayeredWindowAttributes, SetTimer,
        SetWindowPos, ShowWindow, CS_HREDRAW, CS_VREDRAW, HWND_TOPMOST, LWA_ALPHA, MSG,
        SM_CXSCREEN, SM_CYSCREEN, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE,
        SW_SHOWNOACTIVATE, WM_DESTROY, WM_PAINT, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    };

    const W: i32 = 196;
    const H: i32 = 34;

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
    static G_HWND: OnceLock<Mutex<isize>> = OnceLock::new();

    fn hwnd_cell() -> &'static Mutex<isize> {
        G_HWND.get_or_init(|| Mutex::new(0))
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

        pub fn hide(&self) {
            post_state(STATE_HIDDEN);
        }

        pub fn set_level(&self, level: f32) {
            let scaled = (level.clamp(0.0, 1.0) * 1000.0) as u32;
            G_LEVEL.store(scaled, Ordering::SeqCst);
            let hwnd = *hwnd_cell().lock().unwrap() as HWND;
            if !hwnd.is_null() && G_STATE.load(Ordering::SeqCst) == STATE_RECORDING {
                unsafe {
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
            }
        }
    }

    fn post_state(state: u32) {
        G_STATE.store(state, Ordering::SeqCst);
        if state != STATE_RECORDING {
            G_LEVEL.store(0, Ordering::SeqCst);
        }
        let hwnd = *hwnd_cell().lock().unwrap() as HWND;
        if !hwnd.is_null() {
            unsafe {
                PostMessageW(hwnd, WM_USER_STATE, 0, 0);
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

    fn run_osd_window() {
        unsafe {
            let _ = SetProcessDpiAwareness(PROCESS_SYSTEM_DPI_AWARE);
            let hinstance = GetModuleHandleW(std::ptr::null());
            let class_name = wide("AutoVoiceOSD\0");

            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinstance,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: 0 as HBRUSH,
                lpszMenuName: std::ptr::null(),
                lpszClassName: class_name.as_ptr(),
                hIconSm: std::ptr::null_mut(),
            };
            RegisterClassExW(&wc);

            let screen_w = GetSystemMetrics(SM_CXSCREEN);
            let screen_h = GetSystemMetrics(SM_CYSCREEN);
            let x = (screen_w - W) / 2;
            let y = (screen_h - H - 112).max(24);

            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                class_name.as_ptr(),
                wide("auto-voice\0").as_ptr(),
                WS_POPUP,
                x,
                y,
                W,
                H,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                hinstance,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                return;
            }

            let rgn = CreateRoundRectRgn(0, 0, W + 1, H + 1, 26, 26);
            SetWindowRgn(hwnd, rgn, 1);
            SetLayeredWindowAttributes(hwnd, 0, 242, LWA_ALPHA);
            *hwnd_cell().lock().unwrap() = hwnd as isize;

            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) != 0 {
                DispatchMessageW(&msg);
            }
            *hwnd_cell().lock().unwrap() = 0;
        }
    }

    unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, _wp: WPARAM, _lp: LPARAM) -> LRESULT {
        match msg {
            WM_USER_STATE => {
                let state = G_STATE.load(Ordering::SeqCst);
                if state == STATE_HIDDEN {
                    KillTimer(hwnd, TIMER_DONE_HIDE);
                    KillTimer(hwnd, TIMER_ANIM);
                    ShowWindow(hwnd, SW_HIDE);
                } else {
                    SetWindowPos(
                        hwnd,
                        HWND_TOPMOST,
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
                    );
                    ShowWindow(hwnd, SW_SHOWNOACTIVATE);

                    if state == STATE_RECORDING || state == STATE_PROCESSING {
                        KillTimer(hwnd, TIMER_DONE_HIDE);
                        SetTimer(hwnd, TIMER_ANIM, 33, None);
                    } else {
                        KillTimer(hwnd, TIMER_ANIM);
                        KillTimer(hwnd, TIMER_DONE_HIDE);
                        SetTimer(hwnd, TIMER_DONE_HIDE, 2000, None);
                    }
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                0
            }
            WM_TIMER => {
                if _wp == TIMER_ANIM {
                    G_FRAME.fetch_add(1, Ordering::SeqCst);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                } else if _wp == TIMER_DONE_HIDE {
                    KillTimer(hwnd, TIMER_DONE_HIDE);
                    G_STATE.store(STATE_HIDDEN, Ordering::SeqCst);
                    ShowWindow(hwnd, SW_HIDE);
                }
                0
            }
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                let hdc = BeginPaint(hwnd, &mut ps);

                let mem_dc = CreateCompatibleDC(hdc);
                let bmp = CreateCompatibleBitmap(hdc, W, H);
                let old = SelectObject(mem_dc, bmp as HGDIOBJ);

                draw_frame(mem_dc);

                BitBlt(hdc, 0, 0, W, H, mem_dc, 0, 0, SRCCOPY);

                SelectObject(mem_dc, old);
                DeleteObject(bmp as HGDIOBJ);
                DeleteDC(mem_dc);
                EndPaint(hwnd, &ps);
                0
            }
            WM_DESTROY => {
                windows_sys::Win32::UI::WindowsAndMessaging::PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, _wp, _lp),
        }
    }

    unsafe fn draw_frame(dc: HDC) {
        let state = G_STATE.load(Ordering::SeqCst);
        let frame = G_FRAME.load(Ordering::SeqCst);
        let level = G_LEVEL.load(Ordering::SeqCst) as f32 / 1000.0;

        fill_rect(dc, 0, 0, W, H, 0x08, 0x0c, 0x12);
        fill_rect(dc, 1, 1, W - 1, H - 1, 0x0e, 0x14, 0x1d);
        fill_rect(dc, 1, 1, W - 1, 3, 0x22, 0x2d, 0x3d);
        fill_rect(dc, 1, H - 3, W - 1, H - 1, 0x07, 0x09, 0x0f);

        match state {
            STATE_RECORDING => draw_recording(dc, frame, level),
            STATE_PROCESSING => draw_processing(dc, frame),
            STATE_DONE => draw_done(dc, frame),
            _ => {}
        }
    }

    unsafe fn draw_recording(dc: HDC, frame: u32, level: f32) {
        const N: usize = 17;
        const BAR_W: i32 = 6;
        const GAP: i32 = 4;
        const START_X: i32 = 17;
        const CENTER_Y: i32 = H / 2;
        const MIN_H: f32 = 4.0;
        const MAX_H: f32 = 18.0;

        let null_pen = GetStockObject(8) as HGDIOBJ;
        let old_pen = SelectObject(dc, null_pen);
        let t = frame as f32 * 0.22;
        let shaped = level.powf(0.72);

        for i in 0..N {
            let x = START_X + i as i32 * (BAR_W + GAP);
            let center_bias =
                1.0 - (((i as f32 + 0.5) - N as f32 / 2.0).abs() / (N as f32 / 2.0)) * 0.28;
            let ripple = ((t + i as f32 * 0.48).sin() * 0.08 + 0.92).clamp(0.84, 1.0);
            let norm = (shaped * center_bias * ripple).clamp(0.0, 1.0);
            let bar_h = (MIN_H + (MAX_H - MIN_H) * norm) as i32;
            let y = CENTER_Y - bar_h / 2;

            let r = (248.0 - norm * 10.0) as u8;
            let g = (127.0 + norm * 52.0) as u8;
            let b = (88.0 + norm * 38.0) as u8;

            let brush = CreateSolidBrush(colorref(r, g, b));
            let old_brush = SelectObject(dc, brush as HGDIOBJ);
            RoundRect(dc, x, y, x + BAR_W, y + bar_h, 6, 6);
            SelectObject(dc, old_brush);
            DeleteObject(brush as HGDIOBJ);
        }

        SelectObject(dc, old_pen);
        fill_rect(
            dc,
            START_X - 2,
            CENTER_Y + 10,
            W - START_X + 1,
            CENTER_Y + 11,
            0x20,
            0x29,
            0x37,
        );
    }

    unsafe fn draw_processing(dc: HDC, frame: u32) {
        draw_idle_wave(dc, frame, 0xc9, 0xb4, 0x63, 0.34, 0.20);
    }

    unsafe fn draw_done(dc: HDC, frame: u32) {
        draw_idle_wave(dc, frame, 0x6f, 0xde, 0xa0, 0.22, 0.12);
    }

    unsafe fn draw_idle_wave(dc: HDC, frame: u32, r: u8, g: u8, b: u8, base: f32, amp: f32) {
        const N: usize = 17;
        const BAR_W: i32 = 6;
        const GAP: i32 = 4;
        const START_X: i32 = 17;
        const CENTER_Y: i32 = H / 2;
        const MIN_H: f32 = 4.0;
        const MAX_H: f32 = 14.0;

        let null_pen = GetStockObject(8) as HGDIOBJ;
        let old_pen = SelectObject(dc, null_pen);
        let t = frame as f32 * 0.18;

        for i in 0..N {
            let x = START_X + i as i32 * (BAR_W + GAP);
            let center_bias =
                1.0 - (((i as f32 + 0.5) - N as f32 / 2.0).abs() / (N as f32 / 2.0)) * 0.24;
            let norm = (base + ((t + i as f32 * 0.46).sin() * 0.5 + 0.5) * amp) * center_bias;
            let bar_h = (MIN_H + (MAX_H - MIN_H) * norm.clamp(0.0, 1.0)) as i32;
            let y = CENTER_Y - bar_h / 2;

            let brush = CreateSolidBrush(colorref(r, g, b));
            let old_brush = SelectObject(dc, brush as HGDIOBJ);
            RoundRect(dc, x, y, x + BAR_W, y + bar_h, 6, 6);
            SelectObject(dc, old_brush);
            DeleteObject(brush as HGDIOBJ);
        }

        SelectObject(dc, old_pen);
        fill_rect(
            dc,
            START_X - 2,
            CENTER_Y + 10,
            W - START_X + 1,
            CENTER_Y + 11,
            0x20,
            0x29,
            0x37,
        );
    }

    #[inline]
    fn colorref(r: u8, g: u8, b: u8) -> u32 {
        r as u32 | ((g as u32) << 8) | ((b as u32) << 16)
    }

    unsafe fn fill_rect(dc: HDC, x1: i32, y1: i32, x2: i32, y2: i32, r: u8, g: u8, b: u8) {
        let brush = CreateSolidBrush(colorref(r, g, b));
        let rc = RECT {
            left: x1,
            top: y1,
            right: x2,
            bottom: y2,
        };
        FillRect(dc, &rc, brush);
        DeleteObject(brush as HGDIOBJ);
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
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
