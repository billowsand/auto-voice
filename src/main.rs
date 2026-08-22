mod asr;
mod audio;
mod config;
mod diarization;
mod llm;
mod lmstudio;
mod osd;
mod output;
mod platform;
mod runtime;
mod tray;
mod ui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use reqwest::Client;

#[cfg(windows)]
const TRAY_CHILD_ENV: &str = "AUTO_VOICE_TRAY_CHILD";
const DEFAULT_MODEL: &str = "models/sense-voice/model.int8.onnx";
const DEFAULT_TOKENS: &str = "models/sense-voice/tokens.txt";
const DEFAULT_FUNASR_ENCODER_ADAPTOR: &str = "models/funasr-nano/encoder_adaptor.int8.onnx";
const DEFAULT_FUNASR_LLM: &str = "models/funasr-nano/llm.int8.onnx";
const DEFAULT_FUNASR_EMBEDDING: &str = "models/funasr-nano/embedding.int8.onnx";
const DEFAULT_FUNASR_TOKENIZER: &str = "models/funasr-nano";
const DEFAULT_SPEAKER_SEGMENTATION_MODEL: &str =
    "models/speaker-diarization/segmentation/model.int8.onnx";
const DEFAULT_SPEAKER_EMBEDDING_MODEL: &str =
    "models/speaker-diarization/embedding/model.int8.onnx";
const DEFAULT_LM_URL: &str = "http://localhost:1234";
const DEFAULT_LANG: &str = "auto";
const CHUNK_SECS: u64 = 30;

// ── CLI ─────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "auto-voice")]
#[command(about = "Speech-to-text CLI: meeting recordings and live mic input")]
#[command(long_about = "双击启动（无参数）自动进入系统托盘 PTT 模式。")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// LMStudio API base URL (overrides config.toml)
    #[arg(long, global = true)]
    lm_url: Option<String>,

    /// LMStudio model identifier (overrides config.toml).
    /// Use the model name shown in LMStudio's "Model" dropdown, e.g. "qwen3-2b"
    #[arg(long, global = true)]
    lm_model: Option<String>,

    /// Skip LLM correction
    #[arg(long, global = true)]
    no_llm: bool,

    /// ASR backend: "sense-voice" (default) or "funasr-nano"
    #[arg(long, global = true)]
    asr_backend: Option<String>,

    /// Path to SenseVoice ONNX model (overrides config.toml)
    #[arg(long, global = true)]
    model: Option<String>,

    /// Path to tokens.txt (overrides config.toml)
    #[arg(long, global = true)]
    tokens: Option<String>,

    /// FunASR-nano: path to encoder_adaptor.onnx
    #[arg(long, global = true)]
    funasr_encoder_adaptor: Option<String>,

    /// FunASR-nano: path to llm.onnx
    #[arg(long, global = true)]
    funasr_llm: Option<String>,

    /// FunASR-nano: path to embedding.onnx
    #[arg(long, global = true)]
    funasr_embedding: Option<String>,

    /// FunASR-nano: path to tokenizer.json
    #[arg(long, global = true)]
    funasr_tokenizer: Option<String>,

    /// Language hint: auto, zh, en, ja, ko, yue (overrides config.toml)
    #[arg(long, global = true)]
    lang: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Transcribe an audio file to Markdown
    Transcribe {
        /// Input audio file (mp3, wav, flac, ogg, m4a)
        input: std::path::PathBuf,

        /// Output Markdown file (default: <input>.md)
        #[arg(short, long)]
        output: Option<std::path::PathBuf>,

        /// Enable speaker diarization for file transcription
        #[arg(long)]
        diarize: bool,

        /// Expected number of speakers; omit to let clustering infer it
        #[arg(long)]
        speakers: Option<i32>,

        /// Clustering threshold for speaker diarization
        #[arg(long)]
        speaker_threshold: Option<f32>,

        /// Minimum active speech duration for diarization segments
        #[arg(long)]
        speaker_min_duration_on: Option<f32>,

        /// Minimum silence duration before diarization splits segments
        #[arg(long)]
        speaker_min_duration_off: Option<f32>,
    },
    /// Live microphone input → clipboard
    Listen {
        /// Also write to a file
        #[arg(short, long)]
        output: Option<std::path::PathBuf>,

        /// Silence duration (ms) to trigger segment flush (overrides config.toml)
        #[arg(long)]
        vad_silence_ms: Option<u64>,

        /// Energy threshold for speech detection 0.0–1.0 (overrides config.toml)
        #[arg(long)]
        energy_threshold: Option<f32>,

        /// CapsLock push-to-talk: hold CapsLock to record, release to transcribe + paste
        #[arg(long)]
        ptt: bool,
    },
    /// Show device and config info
    Info,
}

// ── Resolved config (CLI > config.toml > hardcoded defaults) ────────────────

pub(crate) struct AppConfig {
    lm_url: String,
    lm_model: String,
    no_llm: bool,
    lmstudio_auto_start: bool,
    lmstudio_model: String,
    lmstudio_context_length: Option<u32>,
    asr_backend: String,
    // SenseVoice
    model: String,
    tokens: String,
    // FunASR-nano
    funasr_encoder_adaptor: String,
    funasr_llm: String,
    funasr_embedding: String,
    funasr_tokenizer: String,
    funasr_itn: bool,
    speaker_segmentation_model: String,
    speaker_embedding_model: String,
    speaker_num_clusters: Option<i32>,
    speaker_threshold: f32,
    speaker_min_duration_on: f32,
    speaker_min_duration_off: f32,
    lang: String,
    energy_threshold: f32,
    vad_silence_ms: u64,
    ptt_key: Option<String>,
    hr_lexicon: Option<String>,
    hr_rule_fsts: Option<String>,
}

impl AppConfig {
    /// Resolve config-file values against the built-in defaults. This is also the path the
    /// settings UI takes when applying a save at runtime, so it must not depend on the CLI.
    pub(crate) fn from_file(file: &config::ConfigFile) -> Self {
        let lm_model = file
            .lm_model
            .clone()
            .unwrap_or_else(|| "local-model".to_string());
        Self {
            lm_url: file
                .lm_url
                .clone()
                .unwrap_or_else(|| DEFAULT_LM_URL.to_string()),
            lm_model: lm_model.clone(),
            no_llm: file.no_llm.unwrap_or(false),
            lmstudio_auto_start: file.lmstudio_auto_start.unwrap_or(false),
            lmstudio_model: file
                .lmstudio_model
                .clone()
                .unwrap_or_else(|| lm_model.clone()),
            lmstudio_context_length: file.lmstudio_context_length,
            asr_backend: file
                .asr_backend
                .clone()
                .unwrap_or_else(|| "sense-voice".to_string()),
            model: file
                .model
                .clone()
                .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            tokens: file
                .tokens
                .clone()
                .unwrap_or_else(|| DEFAULT_TOKENS.to_string()),
            funasr_encoder_adaptor: file
                .funasr_encoder_adaptor
                .clone()
                .unwrap_or_else(|| DEFAULT_FUNASR_ENCODER_ADAPTOR.to_string()),
            funasr_llm: file
                .funasr_llm
                .clone()
                .unwrap_or_else(|| DEFAULT_FUNASR_LLM.to_string()),
            funasr_embedding: file
                .funasr_embedding
                .clone()
                .unwrap_or_else(|| DEFAULT_FUNASR_EMBEDDING.to_string()),
            funasr_tokenizer: file
                .funasr_tokenizer
                .clone()
                .unwrap_or_else(|| DEFAULT_FUNASR_TOKENIZER.to_string()),
            funasr_itn: file.funasr_itn.unwrap_or(true),
            speaker_segmentation_model: file
                .speaker_segmentation_model
                .clone()
                .unwrap_or_else(|| DEFAULT_SPEAKER_SEGMENTATION_MODEL.to_string()),
            speaker_embedding_model: file
                .speaker_embedding_model
                .clone()
                .unwrap_or_else(|| DEFAULT_SPEAKER_EMBEDDING_MODEL.to_string()),
            speaker_num_clusters: file.speaker_num_clusters,
            speaker_threshold: file.speaker_threshold.unwrap_or(0.5),
            speaker_min_duration_on: file.speaker_min_duration_on.unwrap_or(0.2),
            speaker_min_duration_off: file.speaker_min_duration_off.unwrap_or(0.5),
            lang: file
                .lang
                .clone()
                .unwrap_or_else(|| DEFAULT_LANG.to_string()),
            energy_threshold: file.energy_threshold.unwrap_or(0.01),
            vad_silence_ms: file.vad_silence_ms.unwrap_or(800),
            ptt_key: file.ptt_key.clone(),
            hr_lexicon: file.hr_lexicon.clone(),
            hr_rule_fsts: file.hr_rule_fsts.clone(),
        }
    }

    fn resolve(cli: &Cli, file: &config::ConfigFile) -> Self {
        let mut app = Self::from_file(file);
        let lmstudio_follows_lm_model = file.lmstudio_model.is_none();

        if let Some(value) = cli.lm_url.clone() {
            app.lm_url = value;
        }
        if let Some(value) = cli.lm_model.clone() {
            if lmstudio_follows_lm_model {
                app.lmstudio_model = value.clone();
            }
            app.lm_model = value;
        }
        app.no_llm |= cli.no_llm;
        if let Some(value) = cli.asr_backend.clone() {
            app.asr_backend = value;
        }
        if let Some(value) = cli.model.clone() {
            app.model = value;
        }
        if let Some(value) = cli.tokens.clone() {
            app.tokens = value;
        }
        if let Some(value) = cli.funasr_encoder_adaptor.clone() {
            app.funasr_encoder_adaptor = value;
        }
        if let Some(value) = cli.funasr_llm.clone() {
            app.funasr_llm = value;
        }
        if let Some(value) = cli.funasr_embedding.clone() {
            app.funasr_embedding = value;
        }
        if let Some(value) = cli.funasr_tokenizer.clone() {
            app.funasr_tokenizer = value;
        }
        if let Some(value) = cli.lang.clone() {
            app.lang = value;
        }
        app
    }

    pub(crate) fn build_hr_config(&self) -> asr::HrConfig {
        asr::HrConfig {
            lexicon: self.hr_lexicon.clone(),
            rule_fsts: self.hr_rule_fsts.clone(),
        }
    }

    pub(crate) fn build_asr_config(&self) -> asr::AsrConfig {
        match self.asr_backend.as_str() {
            "funasr-nano" => asr::AsrConfig::FunAsrNano {
                encoder_adaptor: self.funasr_encoder_adaptor.clone(),
                llm: self.funasr_llm.clone(),
                embedding: self.funasr_embedding.clone(),
                tokenizer: self.funasr_tokenizer.clone(),
                language: self.lang.clone(),
                itn: self.funasr_itn,
            },
            _ => asr::AsrConfig::SenseVoice {
                model: self.model.clone(),
                tokens: self.tokens.clone(),
                language: self.lang.clone(),
            },
        }
    }

    /// The hot-reloadable view of this config, shared with the push-to-talk worker.
    fn build_runtime(&self, file: &config::ConfigFile) -> runtime::Runtime {
        runtime::Runtime::new(
            runtime::LiveTunables {
                ptt_key: self
                    .ptt_key
                    .clone()
                    .unwrap_or_else(|| "CapsLock".to_string()),
                energy_threshold: self.energy_threshold,
                no_llm: self.no_llm,
                lm_url: self.lm_url.clone(),
                lm_model: self.lm_model.clone(),
                follow_caret: file.overlay_follow_caret.unwrap_or(true),
            },
            self.build_asr_config(),
            self.build_hr_config(),
        )
    }

    fn build_lmstudio_config(&self) -> Option<lmstudio::AutoStartConfig> {
        if self.no_llm || !self.lmstudio_auto_start {
            return None;
        }
        Some(lmstudio::AutoStartConfig {
            base_url: self.lm_url.clone(),
            api_model: self.lm_model.clone(),
            local_model: self.lmstudio_model.clone(),
            context_length: self.lmstudio_context_length,
        })
    }
}

// ── Entry point ──────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // 日志文件路径
    let log_dir = platform::app_log_dir();
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("Failed to create log directory {}", log_dir.display()))?;
    let log_file = log_dir.join("auto-voice.log");

    let file_appender = tracing_appender::rolling::daily(log_dir, "auto-voice.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .with_writer(non_blocking)
        .with_ansi(false) // 文件中不输出 ANSI 颜色码
        .init();

    tracing::info!("Log file: {:?}", log_file);
    tracing::info!("auto-voice starting...");

    let cli = Cli::parse();

    #[cfg(windows)]
    if cli.command.is_none() && std::env::var_os(TRAY_CHILD_ENV).is_none() {
        relaunch_detached_tray_process()?;
        return Ok(());
    }

    let file_cfg = config::ConfigFile::load();
    let app = AppConfig::resolve(&cli, &file_cfg);

    match cli.command {
        // ── 无参数双击启动 → 系统托盘 PTT 模式 ─────────────────────────────
        None => {
            tray::run_tray(tray::TrayConfig {
                runtime: app.build_runtime(&file_cfg),
                lmstudio: app.build_lmstudio_config(),
                settings: file_cfg,
            })?;
        }

        Some(Commands::Transcribe {
            ref input,
            ref output,
            diarize,
            speakers,
            speaker_threshold,
            speaker_min_duration_on,
            speaker_min_duration_off,
        }) => {
            ensure_lmstudio_ready(&app);
            cmd_transcribe(
                &app,
                input,
                output.clone(),
                diarize,
                speakers,
                speaker_threshold,
                speaker_min_duration_on,
                speaker_min_duration_off,
            )
            .await?;
        }
        Some(Commands::Listen {
            output,
            vad_silence_ms,
            energy_threshold,
            ptt,
        }) => {
            ensure_lmstudio_ready(&app);

            if ptt {
                // run_ptt 自己加载模型，这样 CLI 与托盘走同一条热重载路径。
                let runtime = app.build_runtime(&file_cfg);
                tokio::task::block_in_place(|| audio::ptt::run_ptt(&runtime, None))?;
            } else {
                tracing::info!("Loading {} model...", app.asr_backend);
                let engine =
                    asr::AsrEngine::new(&app.build_asr_config(), Some(&app.build_hr_config()))
                        .context("Failed to load ASR model")?;

                let live_cfg = audio::mic::LiveConfig {
                    vad_silence_ms: vad_silence_ms.unwrap_or(app.vad_silence_ms),
                    energy_threshold: energy_threshold.unwrap_or(app.energy_threshold),
                    output_path: output,
                    lm_url: app.lm_url.clone(),
                    lm_model: app.lm_model.clone(),
                    no_llm: app.no_llm,
                };
                tokio::task::block_in_place(|| audio::mic::run_live(&live_cfg, &engine))?;
            }
        }
        Some(Commands::Info) => {
            audio::print_devices()?;
            println!();
            println!("Resolved config:");
            println!("  asr_backend: {}", app.asr_backend);
            match app.asr_backend.as_str() {
                "funasr-nano" => {
                    println!("  funasr_encoder_adaptor: {}", app.funasr_encoder_adaptor);
                    println!("  funasr_llm:             {}", app.funasr_llm);
                    println!("  funasr_embedding:       {}", app.funasr_embedding);
                    println!("  funasr_tokenizer:       {}", app.funasr_tokenizer);
                    println!("  funasr_itn:             {}", app.funasr_itn);
                }
                _ => {
                    println!("  model:  {}", app.model);
                    println!("  tokens: {}", app.tokens);
                }
            }
            println!("  lm_url:   {}", app.lm_url);
            println!("  lm_model: {}", app.lm_model);
            println!("  no_llm:   {}", app.no_llm);
            println!("  lmstudio_auto_start: {}", app.lmstudio_auto_start);
            println!("  lmstudio_model:      {}", app.lmstudio_model);
            println!(
                "  lmstudio_context_length: {}",
                app.lmstudio_context_length
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "default".to_string())
            );
            println!("  lang:   {}", app.lang);
        }
    }

    Ok(())
}

fn ensure_lmstudio_ready(app: &AppConfig) {
    let Some(cfg) = app.build_lmstudio_config() else {
        return;
    };
    if let Err(error) = lmstudio::ensure_ready(&cfg) {
        tracing::warn!(
            "LM Studio auto-start failed; LLM calls will fall back to raw text: {error:#}"
        );
        eprintln!("Warning: LM Studio auto-start failed: {error:#}");
    }
}

#[cfg(windows)]
fn relaunch_detached_tray_process() -> Result<()> {
    use std::os::windows::process::CommandExt;

    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let mut cmd = std::process::Command::new(
        std::env::current_exe().context("Failed to resolve current executable path")?,
    );
    cmd.args(std::env::args_os().skip(1))
        .current_dir(
            std::env::current_dir().context("Failed to resolve current working directory")?,
        )
        .env(TRAY_CHILD_ENV, "1")
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);

    cmd.spawn()
        .context("Failed to relaunch detached tray process")?;
    Ok(())
}

// ── transcribe command ───────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn cmd_transcribe(
    app: &AppConfig,
    input: &std::path::Path,
    output: Option<std::path::PathBuf>,
    diarize: bool,
    speakers: Option<i32>,
    speaker_threshold: Option<f32>,
    speaker_min_duration_on: Option<f32>,
    speaker_min_duration_off: Option<f32>,
) -> Result<()> {
    let out_path = output.unwrap_or_else(|| input.with_extension("md"));

    tracing::info!("Loading {} model...", app.asr_backend);
    let engine = asr::AsrEngine::new(&app.build_asr_config(), Some(&app.build_hr_config()))
        .context(
            "Failed to load ASR model. Make sure model files exist (see --model / --asr-backend)",
        )?;

    tracing::info!("Decoding: {}", input.display());
    let decoded = audio::decode::decode_file(input)?;
    tracing::info!(
        "Audio: {}Hz {}ch  {:.1}s",
        decoded.sample_rate,
        decoded.channels,
        decoded.samples.len() as f32 / decoded.sample_rate as f32 / decoded.channels as f32
    );

    let mono =
        audio::resample::to_mono_16k(&decoded.samples, decoded.sample_rate, decoded.channels)?;
    let duration_secs = mono.len() as f64 / 16000.0;
    tracing::info!("Resampled: {} samples ({:.1}s)", mono.len(), duration_secs);

    let transcript_segments = if diarize {
        tracing::info!("Loading speaker diarization models...");
        let diarization_cfg = diarization::DiarizationConfig {
            segmentation_model: app.speaker_segmentation_model.clone(),
            embedding_model: app.speaker_embedding_model.clone(),
            num_clusters: speakers.or(app.speaker_num_clusters),
            threshold: speaker_threshold.unwrap_or(app.speaker_threshold),
            min_duration_on: speaker_min_duration_on.unwrap_or(app.speaker_min_duration_on),
            min_duration_off: speaker_min_duration_off.unwrap_or(app.speaker_min_duration_off),
        };
        tracing::info!(
            "Diarization config: speakers={:?}, threshold={:.3}, min_on={:.3}, min_off={:.3}",
            diarization_cfg.num_clusters,
            diarization_cfg.threshold,
            diarization_cfg.min_duration_on,
            diarization_cfg.min_duration_off
        );
        let diarization = diarization::DiarizationEngine::new(&diarization_cfg)?;
        if diarization.sample_rate() != 16000 {
            anyhow::bail!(
                "Speaker diarization model expects {}Hz audio; current pipeline resamples to 16000Hz",
                diarization.sample_rate()
            );
        }

        let speaker_segments = diarization.diarize(&mono)?;
        tracing::info!(
            "Speaker diarization produced {} segment(s)",
            speaker_segments.len()
        );
        log_diarization_segments(&speaker_segments);
        transcribe_diarized_segments(app, &engine, &mono, &speaker_segments).await?
    } else {
        transcribe_plain_chunks(app, &engine, &mono).await?
    };

    let source_name = input
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    output::markdown::write_markdown(&transcript_segments, source_name, duration_secs, &out_path)?;
    println!("Saved: {}", out_path.display());

    Ok(())
}

fn fmt_offset(secs: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

fn log_diarization_segments(speaker_segments: &[diarization::SpeakerSegment]) {
    use std::collections::{BTreeMap, BTreeSet};

    let mut speakers = BTreeSet::new();
    let mut durations = BTreeMap::<usize, f32>::new();

    for seg in speaker_segments {
        speakers.insert(seg.speaker);
        *durations.entry(seg.speaker).or_default() += seg.end_sec - seg.start_sec;
        tracing::info!(
            "  diarization segment: {:.2}s -> {:.2}s speaker_{} ({:.2}s)",
            seg.start_sec,
            seg.end_sec,
            seg.speaker,
            seg.end_sec - seg.start_sec
        );
    }

    tracing::info!("Diarization unique speakers: {}", speakers.len());
    for (speaker, duration) in durations {
        tracing::info!(
            "  diarization speaker_{} total duration: {:.2}s",
            speaker,
            duration
        );
    }
}

async fn transcribe_plain_chunks(
    app: &AppConfig,
    engine: &asr::AsrEngine,
    mono: &[f32],
) -> Result<Vec<output::markdown::TranscriptSegment>> {
    let chunk_size = 16000 * CHUNK_SECS as usize;
    let chunks: Vec<&[f32]> = mono.chunks(chunk_size).collect();
    tracing::info!("Transcribing {} chunk(s)...", chunks.len());

    let mut segments = Vec::new();
    for (i, chunk) in chunks.iter().enumerate() {
        let start_sec = i as f64 * CHUNK_SECS as f64;
        let end_sec = start_sec + chunk.len() as f64 / 16000.0;
        tracing::info!("  chunk {}/{}", i + 1, chunks.len());
        let seg = engine.transcribe(chunk)?;
        tracing::info!(
            "    [{}] {}",
            fmt_offset(start_sec as u64),
            if seg.text.is_empty() {
                "(empty)"
            } else {
                &seg.text
            }
        );
        segments.push(output::markdown::TranscriptSegment {
            start_sec,
            end_sec,
            speaker: None,
            text: maybe_correct_text(app, &seg.text).await?,
        });
    }

    Ok(segments)
}

async fn transcribe_diarized_segments(
    app: &AppConfig,
    engine: &asr::AsrEngine,
    mono: &[f32],
    speaker_segments: &[diarization::SpeakerSegment],
) -> Result<Vec<output::markdown::TranscriptSegment>> {
    let max_chunk_samples = 16000 * CHUNK_SECS as usize;
    let mut transcript_segments = Vec::new();

    for seg in speaker_segments {
        let start_sample = ((seg.start_sec as f64) * 16000.0).floor().max(0.0) as usize;
        let end_sample = ((seg.end_sec as f64) * 16000.0).ceil().max(0.0) as usize;
        let start_sample = start_sample.min(mono.len());
        let end_sample = end_sample.min(mono.len());
        if end_sample <= start_sample {
            continue;
        }

        let slice = &mono[start_sample..end_sample];
        for (chunk_idx, chunk) in slice.chunks(max_chunk_samples).enumerate() {
            let sub_start = start_sample + chunk_idx * max_chunk_samples;
            let sub_end = sub_start + chunk.len();
            let start_sec = sub_start as f64 / 16000.0;
            let end_sec = sub_end as f64 / 16000.0;
            let asr_seg = engine.transcribe(chunk)?;
            tracing::info!(
                "    [{}] speaker_{} {}",
                fmt_offset(start_sec as u64),
                seg.speaker,
                if asr_seg.text.is_empty() {
                    "(empty)"
                } else {
                    &asr_seg.text
                }
            );
            transcript_segments.push(output::markdown::TranscriptSegment {
                start_sec,
                end_sec,
                speaker: Some(format!("Speaker {}", seg.speaker + 1)),
                text: maybe_correct_text(app, &asr_seg.text).await?,
            });
        }
    }

    Ok(transcript_segments)
}

async fn maybe_correct_text(app: &AppConfig, text: &str) -> Result<String> {
    if app.no_llm || text.trim().is_empty() {
        return Ok(text.trim().to_string());
    }

    let client = Client::new();
    match llm::correct_text(&client, &app.lm_url, &app.lm_model, text).await {
        Ok(text) => Ok(text.trim().to_string()),
        Err(e) => {
            tracing::warn!("LLM error (skipping): {}", e);
            Ok(text.trim().to_string())
        }
    }
}
