use anyhow::{Context, Result};
use sherpa_onnx::{
    HomophoneReplacerConfig, OfflineFunASRNanoModelConfig, OfflineModelConfig, OfflineRecognizer,
    OfflineRecognizerConfig, OfflineSenseVoiceModelConfig,
};

/// ASR backend selection with all required model paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AsrConfig {
    SenseVoice {
        model: String,
        tokens: String,
        language: String,
    },
    FunAsrNano {
        encoder_adaptor: String,
        llm: String,
        embedding: String,
        tokenizer: String,
        language: String,
        itn: bool,
    },
}

/// A single transcribed segment.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Segment {
    pub text: String,
    pub tokens: Vec<String>,
    /// Token-level timestamps in seconds (if available)
    pub timestamps: Option<Vec<f32>>,
}

/// Optional homophone replacement config (外挂词典 + FST 规则文件路径).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HrConfig {
    /// 发音词典文件路径，例如 "models/hr/lexicon.txt"
    pub lexicon: Option<String>,
    /// FST 规则文件路径，例如 "models/hr/replace.fst"
    pub rule_fsts: Option<String>,
}

impl HrConfig {
    pub fn is_enabled(&self) -> bool {
        self.lexicon.is_some() || self.rule_fsts.is_some()
    }
}

pub struct AsrEngine {
    recognizer: OfflineRecognizer,
}

impl AsrEngine {
    pub fn new(config: &AsrConfig, hr: Option<&HrConfig>) -> Result<Self> {
        let model_config = match config {
            AsrConfig::SenseVoice {
                model,
                tokens,
                language,
            } => OfflineModelConfig {
                sense_voice: OfflineSenseVoiceModelConfig {
                    model: Some(model.clone()),
                    language: Some(language.clone()),
                    use_itn: true,
                },
                tokens: Some(tokens.clone()),
                num_threads: 4,
                debug: false,
                provider: Some("cpu".to_string()),
                ..Default::default()
            },
            AsrConfig::FunAsrNano {
                encoder_adaptor,
                llm,
                embedding,
                tokenizer,
                language,
                itn,
            } => OfflineModelConfig {
                funasr_nano: OfflineFunASRNanoModelConfig {
                    encoder_adaptor: Some(encoder_adaptor.clone()),
                    llm: Some(llm.clone()),
                    embedding: Some(embedding.clone()),
                    tokenizer: Some(tokenizer.clone()),
                    language: Some(language.clone()),
                    itn: if *itn { 1 } else { 0 },
                    ..Default::default()
                },
                num_threads: 4,
                debug: false,
                provider: Some("cpu".to_string()),
                ..Default::default()
            },
        };

        let hr_config = hr
            .filter(|h| h.is_enabled())
            .map(|h| HomophoneReplacerConfig {
                lexicon: h.lexicon.clone(),
                rule_fsts: h.rule_fsts.clone(),
            })
            .unwrap_or_default();

        let recognizer = OfflineRecognizer::create(&OfflineRecognizerConfig {
            model_config,
            hr: hr_config,
            ..Default::default()
        })
        .context("Failed to create ASR recognizer")?;

        Ok(Self { recognizer })
    }

    /// Transcribe a slice of 16kHz mono f32 PCM samples.
    pub fn transcribe(&self, samples: &[f32]) -> Result<Segment> {
        let stream = self.recognizer.create_stream();
        stream.accept_waveform(16000, samples);
        self.recognizer.decode(&stream);

        let result = stream.get_result().context("ASR returned no result")?;

        Ok(Segment {
            text: result.text.trim().to_string(),
            tokens: result.tokens,
            timestamps: result.timestamps,
        })
    }
}
