# 参与贡献

感谢你改进 auto-voice。提交改动前，请先搜索现有 Issue，避免重复工作；较大的功能建议先创建讨论或功能请求。

## 本地开发

要求：Windows、Rust 1.85 或更高版本。模型文件不纳入版本控制，仅运行实际识别功能时需要。

```powershell
git clone https://github.com/billowsand/auto-voice.git
cd auto-voice
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
cargo build --release --locked
```

## Pull Request

- 每个 PR 聚焦一个主题，避免顺带重构无关代码。
- 用户可见的行为变化需要更新 README 或 CHANGELOG。
- 提交前确保格式化、Clippy、测试和 Release 构建通过。
- 不要提交模型、录音、日志、密钥或本机专用配置。
- 说明改动目的、验证方式以及可能影响的 Windows 版本或音频设备。

提交贡献即表示你同意按项目的 [MIT License](LICENSE) 授权你的改动。
