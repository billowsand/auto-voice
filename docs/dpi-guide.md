# Windows DPI 适配指南

本文档说明如何在 Windows 上正确处理 Per-Monitor DPI 缩放。

## 核心概念

### DIP（Device Independent Pixel，设备无关像素）

DIP 是一种与设备无关的坐标单位。在 96 DPI 下，1 DIP = 1 物理像素；在 150% DPI (144 DPI) 下，1 DIP = 1.5 物理像素。

### Direct2D 的 DPI 转换机制

当创建 Direct2D RenderTarget 时，设置 `dpiX` 和 `dpiY` 属性后：
- 你以 DIP 单位绘制坐标
- Direct2D 自动将 DIP 转换为物理像素

公式：`物理像素 = DIP × (DPI / 96)`

## 正确做法

### 1. 窗口尺寸 = 物理像素

窗口使用物理像素创建，因为 Windows 消息和窗口 API 使用物理像素。

```rust
// DPI 150% 时
let dpi = 144;
let scale = dpi as f32 / 96.0; // 1.5
let win_w = (BASE_WIN_W as f32 * scale) as i32; // 540 物理像素
let win_h = (BASE_WIN_H as f32 * scale) as i32; // 120 物理像素
```

### 2. RenderTarget DPI = 实际系统 DPI

```rust
let render_props = D2D1_RENDER_TARGET_PROPERTIES {
    dpiX: dpi as f32,
    dpiY: dpi as f32,
    // ...
};

let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
    pixelSize: D2D_SIZE_U { width: win_w as u32, height: win_h as u32 },
    // ...
};
```

### 3. 绘制坐标始终使用 DIP（不要乘以 scale）

```rust
// 错误：双重缩放
let x = 36.0 * scale * scale; // DIP * scale * scale = 错误

// 正确：DIP 是设计像素，Direct2D 自动处理物理像素转换
let x = 36.0; // DIP
// 144 DPI 下：36 DIP * 1.5 = 54 物理像素 ✓
```

### 4. 字体大小使用 DIP

```rust
// 错误
let font_size = 14.0 * scale;

// 正确
let font_size = 14.0; // DIP，DirectWrite 自动缩放
```

## 计算示例

| DPI | 缩放比 | 窗口尺寸 | RenderTarget DPI | 绘制 360 DIP | 实际物理像素 |
|-----|--------|----------|-----------------|--------------|-------------|
| 96  | 100%   | 360×80   | 96              | 360          | 360         |
| 120 | 125%   | 450×100  | 120             | 360          | 450         |
| 144 | 150%   | 540×120  | 144             | 360          | 540         |
| 192 | 200%   | 720×160  | 192             | 360          | 720         |

## 常见错误

### 错误 1：坐标双重缩放

```rust
// ❌ 错误
let rect = D2D_RECT_F {
    left: x * scale,      // 乘了一次
    top: y * scale,       // ...
    right: (x + w) * scale * scale, // 又乘了一次！
    bottom: (y + h) * scale * scale,
};

// ✓ 正确
let rect = D2D_RECT_F {
    left: x,
    top: y,
    right: x + w,
    bottom: y + h,
};
```

### 错误 2：RenderTarget DPI 与窗口不一致

```rust
// ❌ 错误：RenderTarget 用 96 DPI，但窗口是 150% DPI
let render_props = D2D1_RENDER_TARGET_PROPERTIES {
    dpiX: 96.0, // 硬编码！
    dpiY: 96.0,
    // ...
};

// ✓ 正确：使用实际窗口 DPI
let render_props = D2D1_RENDER_TARGET_PROPERTIES {
    dpiX: dpi as f32, // 从窗口获取的实际 DPI
    dpiY: dpi as f32,
    // ...
};
```

### 错误 3：bitmap DPI 设置错误

```rust
// ❌ 错误：bitmap DPI 不影响缩放
let props = D2D1_BITMAP_PROPERTIES {
    dpiX: 96.0, // 设为与 render target 不同会导致拉伸
    dpiY: 96.0,
    // ...
};

// ✓ 正确：让 Direct2D 处理缩放
// bitmap DPI 表示图片的设计分辨率
// Direct2D 会根据 render target DPI 自动计算拉伸比例
```

## Windows Per-Monitor DPI API

```rust
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetDpiForSystem, SetProcessDpiAwareness, PROCESS_PER_MONITOR_DPI_AWARE};

// 在进程启动时设置 DPI 感知模式
SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE);

// 获取窗口所属显示器的 DPI
let dpi = GetDpiForWindow(hwnd);

// 获取系统 DPI（用于窗口创建时的初始尺寸）
let system_dpi = GetDpiForSystem();
```

## OSD 窗口代码示例

```rust
fn run_osd_window() {
    // 1. 设置 DPI 感知
    unsafe { SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE); }

    // 2. 注册窗口类
    // ...

    // 3. 获取 DPI 并计算窗口物理尺寸
    let dpi = get_system_dpi();
    let scale = dpi as f32 / 96.0;
    let win_w = (BASE_WIN_W as f32 * scale) as i32;
    let win_h = (BASE_WIN_H as f32 * scale) as i32;

    // 4. 创建窗口（物理像素）
    let hwnd = CreateWindowExW(..., x, y, win_w, win_h, ...);

    // 5. 创建 RenderTarget（使用实际 DPI）
    let dpi = GetDpiForWindow(hwnd);
    let render_props = D2D1_RENDER_TARGET_PROPERTIES {
        dpiX: dpi as f32,
        dpiY: dpi as f32,
        // ...
    };
    let target = factory.CreateHwndRenderTarget(&render_props, &hwnd_props);

    // 6. 绘制时使用 DIP（不乘以 scale）
    // 坐标 x=36 表示 36 DIP
    // 144 DPI 下自动变为 54 物理像素
}
```

## 调试技巧

添加日志输出关键 DPI 值：

```rust
tracing::info!("[OSD DPI] Window: {}x{} px, DPI={}, scale={}",
    win_w, win_h, dpi, dpi as f32 / 96.0);
tracing::info!("[OSD DPI] RenderTarget: {}x{} px, DPI={}",
    scaled_w, scaled_h, dpi);
```

## 参考资料

- [MSDN: DPI and Device-Independent Pixels](https://docs.microsoft.com/en-us/windows/win32/api/dxgicommon/)
- [MSDN: High DPI Documentation](https://docs.microsoft.com/en-us/windows/win32/hidpi/high-dpi-desktop-application-development)
