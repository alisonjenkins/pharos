//! The GPU pass with the CPU behind it (spec 039, R12). A device error or a
//! panic in a GPU window sends that window and the rest of its song to the
//! CPU; three in ten minutes keep every song on the CPU until restart.

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::{Duration, Instant};

use crate::service::{DeviceUsed, Model, ModelError};

pub const DEVICE_FALLBACK_TOTAL: &str = "pharos_aligner_device_fallback_total";
/// GPU errors within `LATCH_WINDOW` that turn the GPU off until restart.
pub const LATCH_ERRORS: usize = 3;
pub const LATCH_WINDOW: Duration = Duration::from_secs(600);

/// Recent GPU errors, and whether they have latched the CPU.
#[derive(Debug, Default)]
pub struct Latch {
    errors: VecDeque<Instant>,
    latched: bool,
}

impl Latch {
    /// Counts an error at `now`; true when this one latches.
    pub fn record(&mut self, now: Instant) -> bool {
        self.errors.push_back(now);
        while self
            .errors
            .front()
            .is_some_and(|&t| now.duration_since(t) > LATCH_WINDOW)
        {
            self.errors.pop_front();
        }
        if !self.latched && self.errors.len() >= LATCH_ERRORS {
            self.latched = true;
            return true;
        }
        false
    }

    pub fn latched(&self) -> bool {
        self.latched
    }
}

/// Builds the CPU pass the first time a song needs it, so a healthy GPU
/// service never holds the CPU pass's copied weights.
pub type CpuLoader = Box<dyn FnMut() -> Result<Box<dyn Model>, String> + Send>;

pub struct Fallback {
    gpu: Box<dyn Model>,
    cpu: Option<Box<dyn Model>>,
    load_cpu: CpuLoader,
    latch: Latch,
    /// Why this song left the GPU, once it has.
    song_on_cpu: Option<&'static str>,
}

impl Fallback {
    pub fn new(gpu: Box<dyn Model>, load_cpu: CpuLoader) -> Self {
        Self {
            gpu,
            cpu: None,
            load_cpu,
            latch: Latch::default(),
            song_on_cpu: None,
        }
    }

    fn cpu(&mut self) -> Result<&mut Box<dyn Model>, ModelError> {
        if self.cpu.is_none() {
            let cpu = (self.load_cpu)()
                .map_err(|e| ModelError::Fatal(format!("loading the CPU fallback: {e}")))?;
            self.cpu = Some(cpu);
        }
        self.cpu
            .as_mut()
            .ok_or_else(|| ModelError::Fatal("CPU fallback missing after load".into()))
    }

    /// Moves the rest of this song to the CPU after `error`, counting it.
    fn fall_back(&mut self, error: &str) {
        metrics::counter!(DEVICE_FALLBACK_TOTAL, "reason" => "gpu_error").increment(1);
        let latched = self.latch.record(Instant::now());
        tracing::warn!(error = %error, latched, "GPU forward failed; this song continues on the CPU");
        if latched {
            tracing::error!(
                errors = LATCH_ERRORS,
                window_s = LATCH_WINDOW.as_secs(),
                "GPU failed too often; every song runs on the CPU until restart"
            );
        }
        self.song_on_cpu = Some("gpu_error");
    }
}

impl Model for Fallback {
    fn begin_song(&mut self) {
        self.song_on_cpu = if self.latch.latched() {
            metrics::counter!(DEVICE_FALLBACK_TOTAL, "reason" => "latched").increment(1);
            Some("latched")
        } else {
            None
        };
    }

    fn device(&self) -> DeviceUsed {
        match self.song_on_cpu {
            Some(why) => DeviceUsed {
                device: "cpu",
                reason: why.into(),
            },
            None => self.gpu.device(),
        }
    }

    fn align(
        &mut self,
        pcm: &[f32],
        tokens: &[String],
        lead_star: bool,
    ) -> Result<Vec<(u64, u64)>, ModelError> {
        if self.song_on_cpu.is_none() {
            let gpu = &mut self.gpu;
            match catch_unwind(AssertUnwindSafe(|| gpu.align(pcm, tokens, lead_star))) {
                Ok(Err(ModelError::Device(e))) => self.fall_back(&e),
                Ok(other) => return other,
                Err(panic) => {
                    let what = panic
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                        .unwrap_or_else(|| "a panic without a message".into());
                    self.fall_back(&format!("panicked: {what}"));
                }
            }
        }
        self.cpu()?.align(pcm, tokens, lead_star)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn three_errors_inside_the_window_latch_and_older_ones_expire() {
        let t0 = Instant::now();
        let mut l = Latch::default();
        assert!(!l.record(t0));
        assert!(!l.record(t0 + Duration::from_secs(100)));
        // The first has aged out by now, so this is two in the window.
        assert!(!l.record(t0 + Duration::from_secs(601)));
        assert!(!l.latched());
        assert!(l.record(t0 + Duration::from_secs(650)));
        assert!(l.latched());
        assert!(!l.record(t0 + Duration::from_secs(660)), "latches once");
    }

    /// Answers `(1, 1)` on the GPU and `(2, 2)` on the CPU; the GPU fails
    /// when the window's first sample says so.
    struct Fake(&'static str, Arc<AtomicUsize>);

    impl Model for Fake {
        fn device(&self) -> DeviceUsed {
            DeviceUsed {
                device: self.0,
                reason: "test".into(),
            }
        }

        fn align(
            &mut self,
            pcm: &[f32],
            _: &[String],
            _: bool,
        ) -> Result<Vec<(u64, u64)>, ModelError> {
            self.1.fetch_add(1, Ordering::SeqCst);
            match (self.0, pcm[0]) {
                ("vulkan", 1.0) => Err(ModelError::Device("device lost".into())),
                ("vulkan", 2.0) => panic!("queue submit failed"),
                ("vulkan", _) => Ok(vec![(1, 1)]),
                _ => Ok(vec![(2, 2)]),
            }
        }
    }

    #[test]
    fn a_gpu_error_moves_the_rest_of_the_song_to_the_cpu_and_three_latch() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let (gpu_calls, cpu_calls, loads) = (
            Arc::default(),
            Arc::<AtomicUsize>::default(),
            Arc::new(AtomicUsize::new(0)),
        );
        let (c, l) = (Arc::clone(&cpu_calls), Arc::clone(&loads));
        let mut f = Fallback::new(
            Box::new(Fake("vulkan", Arc::clone(&gpu_calls))),
            Box::new(move || {
                l.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(Fake("cpu", Arc::clone(&c))) as Box<dyn Model>)
            }),
        );
        let t: Vec<String> = vec!["la".into()];
        let ok = [0.0f32];

        f.begin_song();
        assert_eq!(f.align(&ok, &t, false).unwrap(), vec![(1, 1)]);
        assert_eq!(f.device().device, "vulkan");
        assert_eq!(
            loads.load(Ordering::SeqCst),
            0,
            "a healthy GPU never loads the CPU pass"
        );

        // An error: this window and the rest of the song go to the CPU.
        assert_eq!(f.align(&[1.0], &t, false).unwrap(), vec![(2, 2)]);
        assert_eq!(f.align(&ok, &t, false).unwrap(), vec![(2, 2)]);
        assert_eq!(
            f.device(),
            DeviceUsed {
                device: "cpu",
                reason: "gpu_error".into()
            }
        );

        // The next song starts on the GPU again; a panic counts like an error.
        f.begin_song();
        assert_eq!(f.align(&ok, &t, false).unwrap(), vec![(1, 1)]);
        assert_eq!(f.align(&[2.0], &t, false).unwrap(), vec![(2, 2)]);

        // A third error latches: later songs never touch the GPU.
        f.begin_song();
        assert_eq!(f.align(&[1.0], &t, false).unwrap(), vec![(2, 2)]);
        let before = gpu_calls.load(Ordering::SeqCst);
        f.begin_song();
        assert_eq!(f.align(&ok, &t, false).unwrap(), vec![(2, 2)]);
        assert_eq!(gpu_calls.load(Ordering::SeqCst), before);
        assert_eq!(
            f.device(),
            DeviceUsed {
                device: "cpu",
                reason: "latched".into()
            }
        );
        assert_eq!(loads.load(Ordering::SeqCst), 1, "the CPU pass loads once");

        // One snapshot: taking one drains the counters.
        let snapshot = snapshotter.snapshot().into_vec();
        let count = |reason: &str| {
            snapshot
                .iter()
                .find(|(k, ..)| {
                    k.key().name() == DEVICE_FALLBACK_TOTAL
                        && k.key()
                            .labels()
                            .any(|l| l.key() == "reason" && l.value() == reason)
                })
                .map(|(.., v)| v)
        };
        assert_eq!(count("gpu_error"), Some(&DebugValue::Counter(3)));
        assert_eq!(count("latched"), Some(&DebugValue::Counter(1)));
    }
}
