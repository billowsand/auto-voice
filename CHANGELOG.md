# 更新日志

本项目的显著变更会记录在此文件中。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循[语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

## [0.2.0] - 2026-09-19

### Added

- 现代化的自绘设置窗口、首次运行引导和无系统标题栏交互。
- 深海极光、晨雾白瓷、石墨专注三套主题，并同步应用到语音悬浮窗。
- 语音悬浮窗新增“正在优化”状态和独立状态动效。
- 内置 OSD Demo，可预览所有悬浮状态与主题。
- Windows-only CI/CD：标签触发后自动构建 Windows x86_64 压缩包并生成 SHA-256 校验文件。

### Changed

- 悬浮窗固定居中于当前活动显示器的可用工作区，不再跟随对话框或文本光标。
- 设置窗口改用自绘窗口框架，统一最大化、最小化、关闭和拖拽体验。
- 设置页重新组织为总览、说话方式、识别模型、文本优化和外观五个区域。

### Fixed

- 避免将维护者本机的模型路径和配置打入 Release 压缩包。

## [0.1.0] - 2026-07-29

### Added

- SenseVoice 与 FunASR-nano 本地语音识别。
- 系统托盘 PTT 和录音状态 OSD。
- 音频文件转录、实时麦克风识别和说话人分离。
- LM Studio 文本纠错与同音专业词汇替换。

[Unreleased]: https://github.com/billowsand/auto-voice/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/billowsand/auto-voice/releases/tag/v0.2.0
[0.1.0]: https://github.com/billowsand/auto-voice/releases/tag/v0.1.0
