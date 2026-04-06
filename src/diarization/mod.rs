use anyhow::{bail, Context, Result};
use sherpa_onnx::{
    FastClusteringConfig, OfflineSpeakerDiarization, OfflineSpeakerDiarizationConfig,
    OfflineSpeakerSegmentationModelConfig, OfflineSpeakerSegmentationPyannoteModelConfig,
    SpeakerEmbeddingExtractorConfig,
};

#[derive(Debug, Clone)]
pub struct SpeakerSegment {
    pub start_sec: f32,
    pub end_sec: f32,
    pub speaker: usize,
}

#[derive(Debug, Clone)]
pub struct DiarizationConfig {
    pub segmentation_model: String,
    pub embedding_model: String,
    pub num_clusters: Option<i32>,
    pub threshold: f32,
    pub min_duration_on: f32,
    pub min_duration_off: f32,
}

pub struct DiarizationEngine {
    inner: OfflineSpeakerDiarization,
}

impl DiarizationEngine {
    pub fn new(config: &DiarizationConfig) -> Result<Self> {
        if config.segmentation_model.trim().is_empty() {
            bail!("speaker segmentation model path is empty");
        }
        if config.embedding_model.trim().is_empty() {
            bail!("speaker embedding model path is empty");
        }

        let clustering = FastClusteringConfig {
            num_clusters: config.num_clusters.unwrap_or(-1),
            threshold: config.threshold,
            ..Default::default()
        };

        let diarization = OfflineSpeakerDiarization::create(&OfflineSpeakerDiarizationConfig {
            segmentation: OfflineSpeakerSegmentationModelConfig {
                pyannote: OfflineSpeakerSegmentationPyannoteModelConfig {
                    model: Some(config.segmentation_model.clone()),
                },
                ..Default::default()
            },
            embedding: SpeakerEmbeddingExtractorConfig {
                model: Some(config.embedding_model.clone()),
                ..Default::default()
            },
            clustering,
            min_duration_on: config.min_duration_on,
            min_duration_off: config.min_duration_off,
            ..Default::default()
        })
        .context("Failed to create speaker diarization engine")?;

        Ok(Self { inner: diarization })
    }

    pub fn sample_rate(&self) -> u32 {
        self.inner
            .sample_rate()
            .try_into()
            .expect("speaker diarization sample rate should fit in u32")
    }

    pub fn diarize(&self, samples: &[f32]) -> Result<Vec<SpeakerSegment>> {
        let result = self
            .inner
            .process(samples)
            .context("Speaker diarization failed")?;

        let mut segments = Vec::new();
        for seg in result.sort_by_start_time() {
            if seg.end <= seg.start {
                continue;
            }
            segments.push(SpeakerSegment {
                start_sec: seg.start,
                end_sec: seg.end,
                speaker: seg.speaker as usize,
            });
        }

        Ok(segments)
    }
}
