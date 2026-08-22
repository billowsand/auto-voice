use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Values loaded from `config.toml`.
/// All fields are Option — only set fields override CLI defaults.
#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub struct ConfigFile {
    /// The file this value was loaded from. It is runtime metadata, not TOML data.
    #[serde(skip)]
    source_path: Option<PathBuf>,

    /// True once the file was read back from disk, i.e. this is not a fresh install.
    #[serde(skip)]
    loaded_from_disk: bool,

    /// Set by the first-run wizard. Until it is true the wizard takes over the window.
    pub setup_done: Option<bool>,

    /// Pop the overlay next to the text caret of the focused app instead of the screen edge.
    pub overlay_follow_caret: Option<bool>,

    /// Show the transcript building up on the overlay while the hotkey is still held. Costs a
    /// recogniser pass a couple of times a second on a background thread.
    pub overlay_live_preview: Option<bool>,

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
    /// 自动启动本机 LM Studio daemon、API server，并加载目标模型。
    pub lmstudio_auto_start: Option<bool>,
    /// LM Studio 本地模型键（`lms ls` 输出的 modelKey）。默认沿用 lm_model。
    pub lmstudio_model: Option<String>,
    /// 自动加载模型时使用的上下文长度；不填则使用 LM Studio 默认值。
    pub lmstudio_context_length: Option<u32>,
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
        let platform_path = Self::platform_config_path();
        if let Ok(cfg) = Self::try_load(&platform_path) {
            return cfg;
        }
        Self {
            source_path: Some(platform_path),
            ..Self::default()
        }
    }

    fn try_load(path: impl AsRef<std::path::Path>) -> Result<Self, ()> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|_| ())?;
        let mut cfg: Self = toml::from_str(&text).map_err(|e| {
            eprintln!("Warning: config.toml parse error: {}", e);
        })?;
        cfg.source_path = Some(path.to_path_buf());
        cfg.loaded_from_disk = true;
        Ok(cfg)
    }

    /// A config that has been through the first-run wizard. Files written before the wizard
    /// existed count as configured too, so upgrades do not get sent back to step one.
    pub fn is_configured(&self) -> bool {
        self.setup_done.unwrap_or(false)
            || (self.loaded_from_disk
                && (self.ptt_key.is_some() || self.model.is_some() || self.asr_backend.is_some()))
    }

    pub fn save(&self) -> anyhow::Result<PathBuf> {
        let path = self
            .source_path
            .clone()
            .unwrap_or_else(Self::platform_config_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let serialized = toml::to_string_pretty(self)?;
        let temporary = path.with_extension("toml.tmp");
        std::fs::write(&temporary, serialized)?;
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        std::fs::rename(&temporary, &path)?;
        Ok(path)
    }

    pub fn display_path(&self) -> PathBuf {
        self.source_path
            .clone()
            .unwrap_or_else(Self::platform_config_path)
    }

    fn platform_config_path() -> PathBuf {
        directories::ProjectDirs::from("io.github", "billowsand", "auto-voice")
            .map(|dirs| dirs.config_dir().join("config.toml"))
            .unwrap_or_else(|| PathBuf::from("config.toml"))
    }
}

fn dirs_config() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
}

// ── PTT key parsing ──────────────────────────────────────────────────────────

/// Hotkeys offered by the settings UI: spec, label, and why someone would pick it.
pub const PTT_PRESETS: &[(&str, &str, &str)] = &[
    ("CapsLock", "Caps Lock", "单手可及，最省力"),
    ("RightCtrl", "右 Ctrl", "不影响 Caps Lock 切换大小写"),
    ("RightAlt", "右 Alt", "笔记本键盘常用"),
    (
        "LeftCtrl+LeftAlt",
        "左 Ctrl + 左 Alt",
        "组合键，几乎不会误触",
    ),
    ("ScrollLock", "Scroll Lock", "全键盘专用，零冲突"),
];

/// Human readable form of a `ptt_key` spec, e.g. `"LeftCtrl+LeftAlt"` → `"左 Ctrl + 左 Alt"`.
pub fn describe_ptt_key(spec: &str) -> String {
    if let Some((_, label, _)) = PTT_PRESETS.iter().find(|(value, ..)| *value == spec) {
        return (*label).to_owned();
    }
    spec.split('+')
        .map(|token| match token.trim() {
            "CapsLock" => "Caps Lock",
            "LeftCtrl" => "左 Ctrl",
            "RightCtrl" => "右 Ctrl",
            "LeftAlt" => "左 Alt",
            "RightAlt" => "右 Alt",
            "LeftShift" => "左 Shift",
            "RightShift" => "右 Shift",
            "LeftWin" => "左 Win",
            "RightWin" => "右 Win",
            "ScrollLock" => "Scroll Lock",
            other => other,
        })
        .collect::<Vec<_>>()
        .join(" + ")
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_source_path_is_not_serialized() {
        let config = ConfigFile {
            source_path: Some(PathBuf::from("private/location/config.toml")),
            ptt_key: Some("CapsLock".to_owned()),
            ..ConfigFile::default()
        };
        let text = toml::to_string(&config).expect("config should serialize");
        assert!(text.contains("ptt_key = \"CapsLock\""));
        assert!(!text.contains("source_path"));
        assert!(!text.contains("private/location"));
    }
}
