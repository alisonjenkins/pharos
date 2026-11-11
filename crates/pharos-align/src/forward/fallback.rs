//! The GPU pass with the CPU behind it (spec 039, R12). A device error or a
//! panic in a GPU window sends that window and the rest of its song to the
//! CPU; three in ten minutes keep every song on the CPU until restart. A GPU
//! pass that cannot be built again after an idle unload leaves the CPU
//! serving until restart too.

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::line::LineSpans;
use crate::service::{DeviceUsed, Loader, Model, ModelError};

pub const DEVICE_FALLBACK_TOTAL: &str = "pharos_aligner_device_fallback_total";
/// `reason` labels of `DEVICE_FALLBACK_TOTAL`, a dashboard contract.
pub const GPU_ERROR: &str = "gpu_error";
pub const LATCHED: &str = "latched";
pub const RELOAD_FAILED: &str = "reload_failed";
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
    /// The GPU's pool was emptied once after the latch tripped.
    pool_retired: bool,
}

impl Fallback {
    pub fn new(gpu: Box<dyn Model>, load_cpu: CpuLoader) -> Self {
        Self {
            gpu,
            cpu: None,
            load_cpu,
            latch: Latch::default(),
            song_on_cpu: None,
            pool_retired: false,
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
        metrics::counter!(DEVICE_FALLBACK_TOTAL, "reason" => GPU_ERROR).increment(1);
        let latched = self.latch.record(Instant::now());
        tracing::warn!(error = %error, latched, "GPU forward failed; this song continues on the CPU");
        if latched {
            tracing::error!(
                errors = LATCH_ERRORS,
                window_s = LATCH_WINDOW.as_secs(),
                "GPU failed too often; every song runs on the CPU until restart"
            );
        }
        self.song_on_cpu = Some(GPU_ERROR);
    }
}

impl Model for Fallback {
    /// The GPU's pool after each song it ran. The song that trips the latch
    /// still hands the GPU's empty pages back, once, so the CPU songs after
    /// it do not hold them (B1352); later songs never ask the GPU again. A
    /// lost device that panics on the cleanup costs nothing but the reading.
    fn end_song(&mut self) -> Option<crate::service::GpuMemory> {
        if self.latch.latched() {
            if self.pool_retired {
                return None;
            }
            self.pool_retired = true;
        }
        let gpu = &mut self.gpu;
        let (mem, cleaned) = match catch_unwind(AssertUnwindSafe(|| gpu.end_song())) {
            Ok(mem) => (mem, true),
            Err(panic) => {
                tracing::warn!(error = %panic_message(&*panic), "GPU pool cleanup panicked");
                (None, false)
            }
        };
        if self.pool_retired && cleaned {
            tracing::info!(
                kept_mib = mem.map(|m| m.after.reserved >> 20),
                "GPU pool emptied after the latch; it is not asked again until restart"
            );
        }
        mem
    }

    fn begin_song(&mut self) {
        self.song_on_cpu = if self.latch.latched() {
            metrics::counter!(DEVICE_FALLBACK_TOTAL, "reason" => LATCHED).increment(1);
            Some(LATCHED)
        } else {
            self.gpu.begin_song();
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
    ) -> Result<LineSpans, ModelError> {
        if self.song_on_cpu.is_none() {
            let gpu = &mut self.gpu;
            match catch_unwind(AssertUnwindSafe(|| gpu.align(pcm, tokens, lead_star))) {
                Ok(Err(ModelError::Device(e))) => self.fall_back(&e),
                Ok(other) => return other,
                Err(panic) => self.fall_back(&format!("panicked: {}", panic_message(&*panic))),
            }
        }
        self.cpu()?.align(pcm, tokens, lead_star)
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_else(|| "a panic without a message".into())
}

/// The GPU device's loader: `load_gpu` while it works, and `load_cpu` from
/// the first time it errors or panics (a reload after an idle unload can
/// meet a device that has gone), so the service keeps answering. Only a CPU
/// pass that fails as well is an error.
pub fn gpu_or_cpu<G, C>(load_gpu: G, load_cpu: C) -> Loader
where
    G: Fn() -> Result<Box<dyn Model>, String> + Send + Sync + 'static,
    C: Fn() -> Result<Box<dyn Model>, String> + Send + Sync + 'static,
{
    let latched = AtomicBool::new(false);
    Box::new(move || {
        if latched.load(Ordering::SeqCst) {
            return load_cpu();
        }
        let error = match catch_unwind(AssertUnwindSafe(&load_gpu)) {
            Ok(Ok(model)) => return Ok(model),
            Ok(Err(e)) => e,
            Err(panic) => format!("panicked: {}", panic_message(&*panic)),
        };
        metrics::counter!(DEVICE_FALLBACK_TOTAL, "reason" => RELOAD_FAILED).increment(1);
        latched.store(true, Ordering::SeqCst);
        tracing::warn!(error = %error, "loading the GPU pass failed; every song runs on the CPU until restart");
        load_cpu().map_err(|cpu| {
            format!("loading the GPU pass failed ({error}), and the CPU pass: {cpu}")
        })
    })
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

        fn align(&mut self, pcm: &[f32], _: &[String], _: bool) -> Result<LineSpans, ModelError> {
            self.1.fetch_add(1, Ordering::SeqCst);
            match (self.0, pcm[0]) {
                ("vulkan", 1.0) => Err(ModelError::Device("device lost".into())),
                ("vulkan", 2.0) => panic!("queue submit failed"),
                ("vulkan", _) => Ok(vec![(1, 1)].into()),
                _ => Ok(vec![(2, 2)].into()),
            }
        }
    }

    /// A GPU whose every window fails, counting its pool cleanups.
    struct Failing(Arc<AtomicUsize>);

    impl Model for Failing {
        fn align(&mut self, _: &[f32], _: &[String], _: bool) -> Result<LineSpans, ModelError> {
            Err(ModelError::Device("device lost".into()))
        }

        fn end_song(&mut self) -> Option<crate::service::GpuMemory> {
            self.0.fetch_add(1, Ordering::SeqCst);
            let pool = crate::service::GpuPool {
                in_use: 1,
                reserved: 2,
            };
            Some(crate::service::GpuMemory {
                peak: pool,
                after: pool,
            })
        }
    }

    /// A GPU whose every window fails and whose pool cleanup panics.
    struct PanicsOnCleanup;

    impl Model for PanicsOnCleanup {
        fn align(&mut self, _: &[f32], _: &[String], _: bool) -> Result<LineSpans, ModelError> {
            Err(ModelError::Device("device lost".into()))
        }

        fn device(&self) -> DeviceUsed {
            DeviceUsed {
                device: "vulkan",
                reason: "test".into(),
            }
        }

        fn end_song(&mut self) -> Option<crate::service::GpuMemory> {
            panic!("cleanup on a lost device")
        }
    }

    #[test]
    fn a_cleanup_that_panics_costs_only_the_reading() {
        let cpu_calls = Arc::new(AtomicUsize::new(0));
        let mut f = Fallback::new(
            Box::new(PanicsOnCleanup),
            Box::new(move || Ok(Box::new(Fake("cpu", Arc::clone(&cpu_calls))) as Box<dyn Model>)),
        );
        let t: Vec<String> = vec!["la".into()];
        for _ in 1..=4 {
            f.begin_song();
            f.align(&[0.0], &t, false).unwrap();
            assert_eq!(f.end_song(), None);
        }
        assert!(f.latch.latched());
    }

    #[test]
    fn the_song_that_latches_still_empties_the_gpu_pool_once() {
        let cleanups = Arc::new(AtomicUsize::new(0));
        let cpu_calls = Arc::new(AtomicUsize::new(0));
        let mut f = Fallback::new(
            Box::new(Failing(Arc::clone(&cleanups))),
            Box::new(move || Ok(Box::new(Fake("cpu", Arc::clone(&cpu_calls))) as Box<dyn Model>)),
        );
        let t: Vec<String> = vec!["la".into()];
        for song in 1..=3 {
            f.begin_song();
            f.align(&[0.0], &t, false).unwrap();
            assert!(f.end_song().is_some(), "song {song} reports its pool");
            assert_eq!(cleanups.load(Ordering::SeqCst), song);
        }
        assert!(f.latch.latched(), "the third error latches");
        for _ in 0..2 {
            f.begin_song();
            f.align(&[0.0], &t, false).unwrap();
            assert_eq!(f.end_song(), None);
        }
        assert_eq!(
            cleanups.load(Ordering::SeqCst),
            3,
            "after the latch the GPU is never asked again"
        );
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
        assert_eq!(f.align(&ok, &t, false).unwrap().times, vec![(1, 1)]);
        assert_eq!(f.device().device, "vulkan");
        assert_eq!(
            loads.load(Ordering::SeqCst),
            0,
            "a healthy GPU never loads the CPU pass"
        );

        // An error: this window and the rest of the song go to the CPU.
        assert_eq!(f.align(&[1.0], &t, false).unwrap().times, vec![(2, 2)]);
        assert_eq!(f.align(&ok, &t, false).unwrap().times, vec![(2, 2)]);
        assert_eq!(
            f.device(),
            DeviceUsed {
                device: "cpu",
                reason: "gpu_error".into()
            }
        );

        // The next song starts on the GPU again; a panic counts like an error.
        f.begin_song();
        assert_eq!(f.align(&ok, &t, false).unwrap().times, vec![(1, 1)]);
        assert_eq!(f.align(&[2.0], &t, false).unwrap().times, vec![(2, 2)]);

        // A third error latches: later songs never touch the GPU.
        f.begin_song();
        assert_eq!(f.align(&[1.0], &t, false).unwrap().times, vec![(2, 2)]);
        let before = gpu_calls.load(Ordering::SeqCst);
        f.begin_song();
        assert_eq!(f.align(&ok, &t, false).unwrap().times, vec![(2, 2)]);
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

    #[test]
    fn fallback_reasons_are_distinct() {
        let mut labels = vec![GPU_ERROR, LATCHED, RELOAD_FAILED];
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), 3);
    }

    fn fake_loader(
        device: &'static str,
        loads: &Arc<AtomicUsize>,
        fail: Option<bool>,
    ) -> impl Fn() -> Result<Box<dyn Model>, String> + Send + Sync + 'static {
        let loads = Arc::clone(loads);
        move || {
            loads.fetch_add(1, Ordering::SeqCst);
            match fail {
                Some(true) => panic!("device lost"),
                Some(false) => Err("no adapter".into()),
                None => Ok(Box::new(Fake(device, Arc::default())) as Box<dyn Model>),
            }
        }
    }

    #[test]
    fn a_failed_gpu_reload_serves_from_the_cpu_until_restart() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        for panics in [false, true] {
            let recorder = DebuggingRecorder::new();
            let snapshotter = recorder.snapshotter();
            let _guard = metrics::set_default_local_recorder(&recorder);
            let (gpu_loads, cpu_loads) = (Arc::default(), Arc::default());
            let load = gpu_or_cpu(
                fake_loader("vulkan", &gpu_loads, Some(panics)),
                fake_loader("cpu", &cpu_loads, None),
            );
            assert_eq!(load().unwrap().device().device, "cpu", "panics={panics}");
            assert_eq!(load().unwrap().device().device, "cpu");
            assert_eq!(
                gpu_loads.load(Ordering::SeqCst),
                1,
                "the GPU is not retried"
            );
            assert_eq!(cpu_loads.load(Ordering::SeqCst), 2);
            let snapshot = snapshotter.snapshot().into_vec();
            let reload_failed = snapshot.iter().find(|(k, ..)| {
                k.key().name() == DEVICE_FALLBACK_TOTAL
                    && k.key()
                        .labels()
                        .any(|l| l.key() == "reason" && l.value() == RELOAD_FAILED)
            });
            assert_eq!(
                reload_failed.map(|(.., v)| v),
                Some(&DebugValue::Counter(1))
            );
        }
    }

    #[test]
    fn a_working_gpu_loader_is_used_every_time() {
        let (gpu_loads, cpu_loads) = (Arc::default(), Arc::default());
        let load = gpu_or_cpu(
            fake_loader("vulkan", &gpu_loads, None),
            fake_loader("cpu", &cpu_loads, None),
        );
        assert_eq!(load().unwrap().device().device, "vulkan");
        assert_eq!(load().unwrap().device().device, "vulkan");
        assert_eq!(cpu_loads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn only_a_cpu_that_fails_too_is_an_error() {
        let (gpu_loads, cpu_loads) = (Arc::default(), Arc::default());
        let load = gpu_or_cpu(
            fake_loader("vulkan", &gpu_loads, Some(false)),
            fake_loader("cpu", &cpu_loads, Some(false)),
        );
        let e = load().err().unwrap();
        assert_eq!(
            e,
            "loading the GPU pass failed (no adapter), and the CPU pass: no adapter"
        );
    }
}
