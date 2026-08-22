# 更新日志

本项目的显著变更会记录在此文件中。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循[语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

### Added

- 边说边出字：按住快捷键说话时，浮层就会显示已经识别出来的内容，松开后再由 AI 统一整理成最终文本。可在设置 → 浮层里关闭。
- 首次运行引导：三步选定快捷键与文本优化，完成后收进托盘；配置过的安装启动时不再弹窗。
- 设置即时生效：保存后自动应用，切换识别后端/模型只在后台重新加载引擎，不需要重启程序。
- 听写浮层重做：按住快捷键才弹出，跟随光标定位，显示实时波形、转写进度与插入的文字，随后淡出。
- 单击托盘图标即可打开设置中心。
- 可通过 LM Studio CLI 无界面启动本地服务，并自动加载配置的 LLM。
- 基于 eframe/egui 的现代设置中心和跨平台录音状态 OSD。
- Windows、macOS、Linux 构建矩阵及平台能力降级提示。
- 专业化的 GitHub 项目首页、社区文件和 Issue/PR 模板。
- Windows CI 与标签触发的 GitHub Release 自动化。
- Release 压缩包校验和。

### Changed

- 浮层改用逐像素透明合成：圆角边缘不再有锯齿，卡片本身是半透明玻璃，带柔和投影。Windows 上通过 DWM 混合 alpha 通道实现，旧的色键方案仅在个别驱动上作为兜底（`AUTO_VOICE_OVERLAY=colorkey` 可强制启用）。
- 浮层布局重做：状态、音量表与转写文字分区排布，卡片宽高随内容平滑变化，长文本按行滚动。

### Fixed

- 浮层不再以黑色方块常驻屏幕：Windows 上用色键抠掉背景，空闲时把窗口停到屏幕外。
- 同时运行两个 auto-voice 实例时，浮层的窗口设置不会再错误地作用到另一个实例上。
- 一次听写没识别出内容或出错时，浮层会如实提示，而不是显示"已完成"后卡住不让再录。
- 全局键盘钩子回调不再做 Win32 界面调用，避免超过 LowLevelHooksTimeout 被系统摘掉钩子。
- 托盘模式启动时只隐藏自己的控制台窗口，此前可能隐藏其他程序的窗口。
- Linux 上"收进托盘"不再无效：Wayland 下 winit 无法隐藏已映射的窗口，且 Hyprland 会忽略最小化请求；现在 Hyprland 会把设置窗口停到专用 special workspace（兼容新旧两种 `hyprctl dispatch` 语法），其他 Wayland 合成器走最小化请求，X11 沿用原逻辑。已配置的安装启动时也不再闪出窗口。

## [0.1.0] - 2026-07-29

### Added

- SenseVoice 与 FunASR-nano 本地语音识别。
- 系统托盘 PTT 和录音状态 OSD。
- 音频文件转录、实时麦克风识别和说话人分离。
- LM Studio 文本纠错与同音专业词汇替换。

[Unreleased]: https://github.com/billowsand/auto-voice/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/billowsand/auto-voice/releases/tag/v0.1.0
