use serde::Deserialize;
use std::path::PathBuf;

/// Values loaded from `config.toml`.
/// All fields are Option — only set fields override CLI defaults.
#[derive(Deserialize, Default)]
pub struct ConfigFile {
    /// ASR 后端选择: "sense-voice"（默认）或 "funasr-nano"
    pub asr_backend: Option<String>,

    // ── SenseVoice 模型路径 ──────────────────────────────────────────────────
    pub model: Option<String>,
    pub tokens: Option<String>,

    // ── FunASR-nano 模型路径 ─────────────────────────────────────────────────
    pub funasr_encoder_adaptor: Option<String>,
    pub funasr_llm: Option<String>,
    pub funasr_embedding: Option<String>,
    pub funasr_tokenizer: Option<String>,
    /// FunASR-nano 逆文本规范化（ITN），默认开启
    pub funasr_itn: Option<bool>,

    pub speaker_segmentation_model: Option<String>,
    pub speaker_embedding_model: Option<String>,
    pub speaker_num_clusters: Option<i32>,
    pub speaker_threshold: Option<f32>,
    pub speaker_min_duration_on: Option<f32>,
    pub speaker_min_duration_off: Option<f32>,
    pub lm_url: Option<String>,
    pub lang: Option<String>,
    pub lm_model: Option<String>,
    pub no_llm: Option<bool>,
    pub energy_threshold: Option<f32>,
    pub vad_silence_ms: Option<u64>,
    /// PTT 触发键，支持单键或组合键，用 + 分隔。
    /// 示例: "CapsLock" | "LeftCtrl+LeftAlt" | "RightCtrl" | "ScrollLock"
    pub ptt_key: Option<String>,

    // ── 同音字替换（Homophone Replacer）────────────────────────────────────────
    /// 发音词典文件，例如 "models/hr/lexicon.txt"（从 sherpa-onnx 发布页下载）
    pub hr_lexicon: Option<String>,
    /// FST 替换规则文件，例如 "models/hr/replace.fst"（由 tools/build_hr_rules.py 生成）
    pub hr_rule_fsts: Option<String>,
}

impl ConfigFile {
    pub fn load() -> Self {
        if let Ok(cfg) = Self::try_load("config.toml") {
            return cfg;
        }
        if let Some(cfg_dir) = dirs_config() {
            let path = cfg_dir.join("auto-voice").join("config.toml");
            if let Ok(cfg) = Self::try_load(&path) {
                return cfg;
            }
        }
        Self::default()
    }

    fn try_load(path: impl AsRef<std::path::Path>) -> Result<Self, ()> {
        let text = std::fs::read_to_string(path).map_err(|_| ())?;
        toml::from_str(&text).map_err(|e| {
            eprintln!("Warning: config.toml parse error: {}", e);
        })
    }
}

fn dirs_config() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
}

// ── PTT key parsing ──────────────────────────────────────────────────────────

/// Parse a `ptt_key` string like `"LeftCtrl+LeftAlt"` into a list of rdev keys.
/// Returns `None` if any token is unrecognised.
pub fn parse_ptt_keys(spec: &str) -> Option<Vec<rdev::Key>> {
    spec.split('+')
        .map(|token| str_to_rdev_key(token.trim()))
        .collect()
}

fn str_to_rdev_key(s: &str) -> Option<rdev::Key> {
    match s {
        "CapsLock" => Some(rdev::Key::CapsLock),
        "LeftCtrl" => Some(rdev::Key::ControlLeft),
        "RightCtrl" => Some(rdev::Key::ControlRight),
        "LeftAlt" => Some(rdev::Key::Alt),
        "RightAlt" => Some(rdev::Key::AltGr),
        "LeftShift" => Some(rdev::Key::ShiftLeft),
        "RightShift" => Some(rdev::Key::ShiftRight),
        "LeftWin" => Some(rdev::Key::MetaLeft),
        "RightWin" => Some(rdev::Key::MetaRight),
        "ScrollLock" => Some(rdev::Key::ScrollLock),
        "Insert" => Some(rdev::Key::Insert),
        "Pause" => Some(rdev::Key::Pause),
        other => {
            eprintln!("Warning: unknown ptt_key token \"{}\"", other);
            None
        }
    }
}
