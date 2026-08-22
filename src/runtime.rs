//! Settings shared live between the desktop UI and the push-to-talk worker.
//!
//! Saving in the UI calls [`Runtime::apply`]; the worker picks the new values up on its next
//! loop iteration. Only a model/backend change costs anything, and that is a background reload
//! of the ASR engine rather than an application restart.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::asr::{AsrConfig, HrConfig};
use crate::config::ConfigFile;

/// Values the worker re-reads on every push-to-talk cycle.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveTunables {
    pub ptt_key: String,
    pub input_device: Option<String>,
    pub energy_threshold: f32,
    pub no_llm: bool,
    pub lm_url: String,
    pub lm_model: String,
    pub follow_caret: bool,
    pub live_preview: bool,
}

/// What the ASR engine is doing, surfaced in the settings header and the overlay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineStatus {
    Loading,
    Ready,
    Failed(String),
}

impl EngineStatus {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

struct Inner {
    live: RwLock<LiveTunables>,
    ptt_keys: RwLock<Vec<rdev::Key>>,
    desired_asr: Mutex<(AsrConfig, HrConfig)>,
    /// Bumped whenever `desired_asr` changes; the worker reloads until it catches up.
    requested: AtomicU64,
    applied: AtomicU64,
    status: RwLock<EngineStatus>,
    shutdown: std::sync::atomic::AtomicBool,
}

#[derive(Clone)]
pub struct Runtime {
    inner: Arc<Inner>,
}

impl Runtime {
    pub fn new(live: LiveTunables, asr: AsrConfig, hr: HrConfig) -> Self {
        let ptt_keys = crate::config::parse_ptt_keys(&live.ptt_key).unwrap_or_default();
        Self {
            inner: Arc::new(Inner {
                live: RwLock::new(live),
                ptt_keys: RwLock::new(ptt_keys),
                desired_asr: Mutex::new((asr, hr)),
                requested: AtomicU64::new(1),
                applied: AtomicU64::new(0),
                status: RwLock::new(EngineStatus::Loading),
                shutdown: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    pub fn live(&self) -> LiveTunables {
        self.read(&self.inner.live).clone()
    }

    /// Keys the global hook compares against. Empty means the spec was unparsable, in which
    /// case the worker keeps its previous keys rather than becoming untriggerable.
    pub fn ptt_keys(&self) -> Vec<rdev::Key> {
        self.read(&self.inner.ptt_keys).clone()
    }

    pub fn status(&self) -> EngineStatus {
        self.read(&self.inner.status).clone()
    }

    pub fn set_status(&self, status: EngineStatus) {
        *self.write(&self.inner.status) = status;
    }

    pub fn request_shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
    }

    pub fn shutdown_requested(&self) -> bool {
        self.inner.shutdown.load(Ordering::SeqCst)
    }

    /// Fold a freshly saved config file into the live values. Returns true when the ASR engine
    /// has to be rebuilt, which the caller may want to tell the user about.
    pub fn apply(&self, file: &ConfigFile) -> bool {
        let resolved = crate::AppConfig::from_file(file);
        let live = LiveTunables {
            ptt_key: resolved
                .ptt_key
                .clone()
                .unwrap_or_else(|| "CapsLock".into()),
            input_device: resolved.input_device.clone(),
            energy_threshold: resolved.energy_threshold,
            no_llm: resolved.no_llm,
            lm_url: resolved.lm_url.clone(),
            lm_model: resolved.lm_model.clone(),
            follow_caret: file.overlay_follow_caret.unwrap_or(true),
            live_preview: file.overlay_live_preview.unwrap_or(true),
        };
        if let Some(keys) = crate::config::parse_ptt_keys(&live.ptt_key) {
            *self.write(&self.inner.ptt_keys) = keys;
        } else {
            tracing::warn!("Ignoring unparsable ptt_key \"{}\"", live.ptt_key);
        }
        *self.write(&self.inner.live) = live;

        let desired = (resolved.build_asr_config(), resolved.build_hr_config());
        let mut slot = self
            .inner
            .desired_asr
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *slot == desired {
            return false;
        }
        *slot = desired;
        drop(slot);
        self.inner.requested.fetch_add(1, Ordering::SeqCst);
        true
    }

    /// The engine config the worker still has to load, if it is behind.
    pub fn pending_asr(&self) -> Option<(u64, AsrConfig, HrConfig)> {
        let requested = self.inner.requested.load(Ordering::SeqCst);
        if requested == self.inner.applied.load(Ordering::SeqCst) {
            return None;
        }
        let slot = self
            .inner
            .desired_asr
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Some((requested, slot.0.clone(), slot.1.clone()))
    }

    pub fn mark_asr_applied(&self, generation: u64) {
        self.inner.applied.store(generation, Ordering::SeqCst);
    }

    fn read<'a, T>(&self, lock: &'a RwLock<T>) -> std::sync::RwLockReadGuard<'a, T> {
        lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write<'a, T>(&self, lock: &'a RwLock<T>) -> std::sync::RwLockWriteGuard<'a, T> {
        lock.write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> Runtime {
        Runtime::new(
            LiveTunables {
                ptt_key: "CapsLock".into(),
                input_device: None,
                energy_threshold: 0.01,
                no_llm: false,
                lm_url: "http://localhost:1234".into(),
                lm_model: "local-model".into(),
                follow_caret: true,
                live_preview: true,
            },
            // Same values `AppConfig::from_file` derives from an empty config, so that only a
            // deliberate change in a test shows up as a reload.
            AsrConfig::SenseVoice {
                model: crate::DEFAULT_MODEL.into(),
                tokens: crate::DEFAULT_TOKENS.into(),
                language: crate::DEFAULT_LANG.into(),
            },
            HrConfig::default(),
        )
    }

    #[test]
    fn tuning_changes_do_not_trigger_a_model_reload() {
        let runtime = runtime();
        runtime.mark_asr_applied(runtime.pending_asr().expect("initial load").0);

        let mut config = ConfigFile::default();
        config.energy_threshold = Some(0.05);
        config.ptt_key = Some("RightCtrl".to_owned());
        config.input_device = Some("Studio microphone".to_owned());
        assert!(!runtime.apply(&config));
        assert!(runtime.pending_asr().is_none());
        assert_eq!(runtime.live().energy_threshold, 0.05);
        assert_eq!(
            runtime.live().input_device.as_deref(),
            Some("Studio microphone")
        );
        assert_eq!(runtime.ptt_keys(), vec![rdev::Key::ControlRight]);
    }

    #[test]
    fn switching_backend_queues_one_reload() {
        let runtime = runtime();
        runtime.mark_asr_applied(runtime.pending_asr().expect("initial load").0);

        let mut config = ConfigFile::default();
        config.asr_backend = Some("funasr-nano".to_owned());
        assert!(runtime.apply(&config));
        let (generation, asr, _) = runtime.pending_asr().expect("reload queued");
        assert!(matches!(asr, AsrConfig::FunAsrNano { .. }));
        runtime.mark_asr_applied(generation);
        assert!(runtime.pending_asr().is_none());
    }

    #[test]
    fn an_unparsable_hotkey_keeps_the_previous_binding() {
        let runtime = runtime();
        let mut config = ConfigFile::default();
        config.ptt_key = Some("F13".to_owned());
        runtime.apply(&config);
        assert_eq!(runtime.ptt_keys(), vec![rdev::Key::CapsLock]);
    }
}
