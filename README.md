# auto-voice

错错错错错……

## 构建

```bash
# 需要 Rust 工具链和 sherpa-onnx 预编译库
cargo build --release
```

Release 构建已开启 LTO 和符号裁剪，生成的二进制体积更小。

## 模型准备

在项目根目录创建 `models/` 目录，放入以下模型文件：

```
models/
├── sense-voice/           # SenseVoice 后端
│   ├── model.int8.onnx
│   └── tokens.txt
├── funasr-nano/           # FunASR-nano 后端
│   ├── encoder_adaptor.int8.onnx
│   ├── llm.int8.onnx
│   ├── embedding.int8.onnx
│   ├── merges.txt
│   ├── tokenizer.json
│   └── vocab.json
├── speaker-diarization/   # 说话人分离
│   ├── sherpa-onnx-pyannote-segmentation-3-0/
│   │   └── model.int8.onnx
│   └── 3dspeaker/
│       └── 3dspeaker_speech_eres2net_large_sv_zh-cn_3dspeaker_16k.onnx
└── hr/                    # 同音字替换（可选）
    ├── lexicon.txt
    └── replace.fst
```

模型可从 [sherpa-onnx releases](https://github.com/k2-fsa/sherpa-onnx/releases) 下载。

## 配置

编辑项目根目录的 `config.toml`（首次运行会使用内置默认值）：

```toml
# ASR 后端: "sense-voice"（默认）或 "funasr-nano"
asr_backend = "sense-voice"

# SenseVoice 模型路径
model  = "models/sense-voice/model.int8.onnx"
tokens = "models/sense-voice/tokens.txt"

# 语言: auto, zh, en, ja, ko, yue
lang = "auto"

# LLM 纠错（需要 LM Studio）
lm_url   = "http://localhost:1234"
lm_model = "gemma-4-e2b-it"
no_llm   = true   # 设为 false 启用 LLM 纠错

# PTT 按键（支持组合键，用 + 分隔）
ptt_key = "LeftAlt"

# VAD 参数
energy_threshold = 0.01
vad_silence_ms   = 800

# 说话人分离模型路径
speaker_segmentation_model = "models/speaker-diarization/sherpa-onnx-pyannote-segmentation-3-0/model.int8.onnx"
speaker_embedding_model    = "models/speaker-diarization/3dspeaker/3dspeaker_speech_eres2net_large_sv_zh-cn_3dspeaker_16k.onnx"
```

配置文件也可放在 `%APPDATA%/auto-voice/config.toml`。

## 使用方法

### 系统托盘 PTT 模式

```bash
# 双击 auto-voice.exe 或无参数启动，自动驻留系统托盘
auto-voice
```

按住配置的 PTT 键录音，松开后自动转录并粘贴。

### 转录音频文件

```bash
# 基本转录
auto-voice transcribe meeting.mp3

# 指定输出文件
auto-voice transcribe meeting.mp3 -o notes.md

# 启用说话人分离
auto-voice transcribe meeting.mp3 --diarize

# 指定说话人数量
auto-voice transcribe meeting.mp3 --diarize --speakers 3
```

### 实时麦克风识别

```bash
# VAD 自动分段模式
auto-voice listen

# 输出到文件
auto-voice listen -o live.txt

# PTT 模式
auto-voice listen --ptt
```

### 查看设备与配置信息

```bash
auto-voice info
```

### 命令行参数

所有命令均支持以下全局参数（覆盖 config.toml）：

| 参数 | 说明 |
|------|------|
| `--lm-url <URL>` | LM Studio API 地址 |
| `--lm-model <NAME>` | LM Studio 模型名 |
| `--no-llm` | 跳过 LLM 纠错 |
| `--asr-backend <NAME>` | ASR 后端：`sense-voice` 或 `funasr-nano` |
| `--lang <LANG>` | 语言：`auto`/`zh`/`en`/`ja`/`ko`/`yue` |
| `--model <PATH>` | SenseVoice 模型路径 |
| `--tokens <PATH>` | tokens.txt 路径 |

## 同音字替换

编辑 `tools/build_hr_rules.py` 中的 `RULES` 字典添加专业词汇规则：

```python
RULES = {
    "zhi4neng2ji4suan4": "智能计算",
}
```

运行脚本生成 FST 规则文件：

```bash
pip install pynini
python tools/build_hr_rules.py
```

然后在 `config.toml` 中配置：

```toml
hr_lexicon   = "models/hr/lexicon.txt"
hr_rule_fsts = "models/hr/replace.fst"
```

`lexicon.txt` 需从 [sherpa-onnx hr-files](https://github.com/k2-fsa/sherpa-onnx/releases/tag/hr-files) 下载。

## 技术栈

- **Rust** — 主语言
- **sherpa-onnx** — 语音识别推理引擎（ONNX Runtime）
- **symphonia** — 音频解码（MP3/WAV/FLAC/OGG/AAC）
- **rubato** — 音频重采样
- **cpal** — 麦克风采集（Windows WASAPI）
- **rdev** — 全局键盘钩子
- **tray-icon** — 系统托盘

## 许可证

请参阅各模型的开源许可证。
