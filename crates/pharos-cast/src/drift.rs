//! Pure drift planner for a cast receiver. No IO, no clocks: time arrives as
//! `now_ms`, and the planner only decides what to correct, never performs it.

/// What the group says the receiver should be doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub position_ms: i64,
    pub playing: bool,
}

/// The last receiver report. `position_ms` was true `age_ms` ago.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observed {
    pub position_ms: i64,
    pub is_paused: bool,
    pub age_ms: u64,
}

/// Tuning knobs. Defaults are provisional until the real-TV run (T048).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannerConfig {
    pub threshold_ms: u64,
    pub cooldown_ms: u64,
    pub seek_lead_ms: u64,
}

impl Default for PlannerConfig {
    fn default() -> Self {
        Self {
            threshold_ms: 1500,
            cooldown_ms: 5000,
            seek_lead_ms: 0,
        }
    }
}

/// Planner memory between calls: when the last non-None action was emitted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlannerState {
    last_correction_at_ms: Option<u64>,
}

impl PlannerState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// The correction to apply to the receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Seek { to_ms: i64 },
    Pause,
    Resume,
}

/// Decide one correction. Mutates `state` only when returning a non-None action.
pub fn plan(
    target: &Target,
    observed: &Observed,
    now_ms: u64,
    cfg: &PlannerConfig,
    state: &mut PlannerState,
) -> Action {
    let allowed = cooldown_elapsed(state, now_ms, cfg);

    // State mismatch first. A flip is corrected once, then respects the cooldown
    // so a receiver that keeps flipping is not hammered.
    let mismatch = if target.playing && observed.is_paused {
        Some(Action::Resume)
    } else if !target.playing && !observed.is_paused {
        Some(Action::Pause)
    } else {
        None
    };
    if let Some(action) = mismatch {
        return commit(action, allowed, now_ms, state);
    }

    let interpolated = if observed.is_paused {
        observed.position_ms
    } else {
        observed
            .position_ms
            .saturating_add(i64::try_from(observed.age_ms).unwrap_or(i64::MAX))
    };
    let drift = interpolated.saturating_sub(target.position_ms);
    if drift.unsigned_abs() > cfg.threshold_ms {
        let lead = if target.playing {
            i64::try_from(cfg.seek_lead_ms).unwrap_or(i64::MAX)
        } else {
            0
        };
        let to_ms = target.position_ms.saturating_add(lead).max(0);
        return commit(Action::Seek { to_ms }, allowed, now_ms, state);
    }

    Action::None
}

/// True when no correction was made yet, or the cooldown has fully elapsed.
/// A `now_ms` that went backwards yields false: treated as not elapsed.
fn cooldown_elapsed(state: &PlannerState, now_ms: u64, cfg: &PlannerConfig) -> bool {
    match state.last_correction_at_ms {
        None => true,
        Some(last) => now_ms
            .checked_sub(last)
            .is_some_and(|since| since >= cfg.cooldown_ms),
    }
}

/// Return `action` and record it, if the cooldown allows it; otherwise `None`
/// with state untouched.
fn commit(action: Action, allowed: bool, now_ms: u64, state: &mut PlannerState) -> Action {
    if !allowed {
        return Action::None;
    }
    state.last_correction_at_ms = Some(now_ms);
    action
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const CFG: PlannerConfig = PlannerConfig {
        threshold_ms: 1500,
        cooldown_ms: 5000,
        seek_lead_ms: 0,
    };

    fn tgt(position_ms: i64, playing: bool) -> Target {
        Target {
            position_ms,
            playing,
        }
    }

    fn obs(position_ms: i64, is_paused: bool, age_ms: u64) -> Observed {
        Observed {
            position_ms,
            is_paused,
            age_ms,
        }
    }

    /// State with a correction already made at `at`.
    fn after(at: u64) -> PlannerState {
        PlannerState {
            last_correction_at_ms: Some(at),
        }
    }

    #[test]
    fn default_config_matches_provisional_values() {
        let cfg = PlannerConfig::default();
        assert_eq!(cfg.threshold_ms, 1500);
        assert_eq!(cfg.cooldown_ms, 5000);
        assert_eq!(cfg.seek_lead_ms, 0);
    }

    #[test]
    fn table_of_rules() {
        // (name, target, observed, now, state, expected)
        let cases: Vec<(&str, Target, Observed, u64, PlannerState, Action)> = vec![
            (
                "in sync, both playing: no action",
                tgt(10_000, true),
                obs(10_000, false, 0),
                100_000,
                PlannerState::default(),
                Action::None,
            ),
            (
                "drift exactly threshold: no seek",
                tgt(10_000, true),
                obs(11_500, false, 0),
                100_000,
                PlannerState::default(),
                Action::None,
            ),
            (
                "drift exactly threshold, behind: no seek",
                tgt(10_000, true),
                obs(8_500, false, 0),
                100_000,
                PlannerState::default(),
                Action::None,
            ),
            (
                "drift one over threshold, ahead: seek back",
                tgt(10_000, true),
                obs(11_501, false, 0),
                100_000,
                PlannerState::default(),
                Action::Seek { to_ms: 10_000 },
            ),
            (
                "drift one over threshold, behind: seek forward",
                tgt(10_000, true),
                obs(8_499, false, 0),
                100_000,
                PlannerState::default(),
                Action::Seek { to_ms: 10_000 },
            ),
            (
                "state mismatch: playing target, paused receiver -> Resume",
                tgt(10_000, true),
                obs(10_000, true, 0),
                100_000,
                PlannerState::default(),
                Action::Resume,
            ),
            (
                "state mismatch: paused target, playing receiver -> Pause",
                tgt(10_000, false),
                obs(10_000, false, 0),
                100_000,
                PlannerState::default(),
                Action::Pause,
            ),
            (
                "paused both, in sync: no action",
                tgt(10_000, false),
                obs(10_000, true, 0),
                100_000,
                PlannerState::default(),
                Action::None,
            ),
            (
                "paused target, receiver drifted: seek to target, no lead",
                tgt(10_000, false),
                obs(20_000, true, 0),
                100_000,
                PlannerState::default(),
                Action::Seek { to_ms: 10_000 },
            ),
        ];
        for (name, t, o, now, mut st, expected) in cases {
            let got = plan(&t, &o, now, &CFG, &mut st);
            assert_eq!(got, expected, "case: {name}");
        }
    }

    #[test]
    fn paused_receiver_position_is_not_interpolated() {
        // Playing receiver reported 10_000 nine seconds ago: interpolated to
        // 19_000, drift 9_000 -> seek. Paused receiver: 10_000 is the truth.
        let mut st = PlannerState::default();
        let got = plan(
            &tgt(10_000, true),
            &obs(10_000, false, 9_000),
            0,
            &CFG,
            &mut st,
        );
        assert_eq!(got, Action::Seek { to_ms: 10_000 });

        let mut st = PlannerState::default();
        let got = plan(
            &tgt(10_000, false),
            &obs(10_000, true, 9_000),
            0,
            &CFG,
            &mut st,
        );
        assert_eq!(got, Action::None);
    }

    #[test]
    fn playing_receiver_interpolation_absorbs_report_age() {
        // Reported 10_000 two seconds ago while playing: interpolated 12_000.
        // Drift vs target 10_000 is 2_000 > 1500 -> seek.
        let mut st = PlannerState::default();
        let got = plan(
            &tgt(10_000, true),
            &obs(10_000, false, 2_000),
            0,
            &CFG,
            &mut st,
        );
        assert_eq!(got, Action::Seek { to_ms: 10_000 });

        // Same report, age 1_000: interpolated 11_000, drift 1_000 -> none.
        let mut st = PlannerState::default();
        let got = plan(
            &tgt(10_000, true),
            &obs(10_000, false, 1_000),
            0,
            &CFG,
            &mut st,
        );
        assert_eq!(got, Action::None);
    }

    #[test]
    fn seek_lead_applies_only_when_playing() {
        let cfg = PlannerConfig {
            seek_lead_ms: 300,
            ..CFG
        };
        let mut st = PlannerState::default();
        let got = plan(&tgt(10_000, true), &obs(30_000, false, 0), 0, &cfg, &mut st);
        assert_eq!(got, Action::Seek { to_ms: 10_300 });

        let mut st = PlannerState::default();
        let got = plan(&tgt(10_000, false), &obs(30_000, true, 0), 0, &cfg, &mut st);
        assert_eq!(got, Action::Seek { to_ms: 10_000 });
    }

    #[test]
    fn seek_target_clamps_to_zero() {
        let mut st = PlannerState::default();
        let got = plan(
            &tgt(-50_000, false),
            &obs(10_000, true, 0),
            0,
            &CFG,
            &mut st,
        );
        assert_eq!(got, Action::Seek { to_ms: 0 });
    }

    #[test]
    fn first_mismatch_correction_ignores_cooldown() {
        // Fresh state, now = 0: first correction is allowed even though
        // cooldown (5000) would otherwise be measured from nothing.
        let mut st = PlannerState::default();
        let got = plan(&tgt(10_000, true), &obs(10_000, true, 0), 0, &CFG, &mut st);
        assert_eq!(got, Action::Resume);
        assert_eq!(st, after(0));
    }

    #[test]
    fn mismatch_respects_cooldown_after_first_correction() {
        let mut st = after(100_000);
        let got = plan(
            &tgt(10_000, true),
            &obs(10_000, true, 0),
            102_000,
            &CFG,
            &mut st,
        );
        assert_eq!(got, Action::None);
        assert_eq!(st, after(100_000), "None must not touch state");

        let got = plan(
            &tgt(10_000, true),
            &obs(10_000, true, 0),
            105_000,
            &CFG,
            &mut st,
        );
        assert_eq!(got, Action::Resume, "cooldown exactly elapsed");
        assert_eq!(st, after(105_000));
    }

    #[test]
    fn drift_seek_respects_cooldown() {
        let mut st = after(100_000);
        let got = plan(
            &tgt(10_000, true),
            &obs(20_000, false, 0),
            104_999,
            &CFG,
            &mut st,
        );
        assert_eq!(got, Action::None);

        let got = plan(
            &tgt(10_000, true),
            &obs(20_000, false, 0),
            105_000,
            &CFG,
            &mut st,
        );
        assert_eq!(
            got,
            Action::Seek { to_ms: 10_000 },
            "cooldown exactly elapsed"
        );
        assert_eq!(st, after(105_000));
    }

    #[test]
    fn none_leaves_state_untouched() {
        let mut st = PlannerState::default();
        let got = plan(
            &tgt(10_000, true),
            &obs(10_000, false, 0),
            42,
            &CFG,
            &mut st,
        );
        assert_eq!(got, Action::None);
        assert_eq!(st, PlannerState::default());
    }

    #[test]
    fn now_going_backwards_does_not_panic_and_blocks() {
        let mut st = after(100_000);
        let got = plan(&tgt(10_000, true), &obs(20_000, false, 0), 1, &CFG, &mut st);
        assert_eq!(
            got,
            Action::None,
            "now < last correction: cooldown not elapsed"
        );
        assert_eq!(st, after(100_000));

        let got = plan(&tgt(10_000, true), &obs(10_000, true, 0), 1, &CFG, &mut st);
        assert_eq!(got, Action::None);
    }

    #[test]
    fn extreme_inputs_do_not_panic() {
        let mut st = PlannerState::default();
        let got = plan(
            &tgt(i64::MIN, true),
            &obs(i64::MAX, false, u64::MAX),
            u64::MAX,
            &PlannerConfig {
                threshold_ms: 0,
                cooldown_ms: 0,
                seek_lead_ms: u64::MAX,
            },
            &mut st,
        );
        // MIN + saturated lead (i64::MAX) = -1; the seek target is never negative
        // only because the clamp runs after the lead is added.
        assert_eq!(got, Action::Seek { to_ms: 0 });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    fn arb_i64() -> impl Strategy<Value = i64> {
        prop_oneof![
            4 => any::<i64>(),
            1 => Just(i64::MIN),
            1 => Just(i64::MAX),
            1 => Just(0i64),
        ]
    }

    fn arb_u64() -> impl Strategy<Value = u64> {
        prop_oneof![
            4 => any::<u64>(),
            1 => Just(u64::MIN),
            1 => Just(u64::MAX),
        ]
    }

    fn arb_cfg() -> impl Strategy<Value = PlannerConfig> {
        (arb_u64(), arb_u64(), arb_u64()).prop_map(|(threshold_ms, cooldown_ms, seek_lead_ms)| {
            PlannerConfig {
                threshold_ms,
                cooldown_ms,
                seek_lead_ms,
            }
        })
    }

    fn arb_target() -> impl Strategy<Value = Target> {
        (arb_i64(), any::<bool>()).prop_map(|(position_ms, playing)| Target {
            position_ms,
            playing,
        })
    }

    fn arb_observed() -> impl Strategy<Value = Observed> {
        (arb_i64(), any::<bool>(), arb_u64()).prop_map(|(position_ms, is_paused, age_ms)| {
            Observed {
                position_ms,
                is_paused,
                age_ms,
            }
        })
    }

    proptest! {
        // (d) no panic for arbitrary extreme inputs; (c) seek target never negative.
        #[test]
        fn never_panics_and_seek_never_negative(
            t in arb_target(),
            o in arb_observed(),
            now in arb_u64(),
            cfg in arb_cfg(),
            last in proptest::option::of(arb_u64()),
        ) {
            let mut st = PlannerState { last_correction_at_ms: last };
            if let Action::Seek { to_ms } = plan(&t, &o, now, &cfg, &mut st) {
                prop_assert!(to_ms >= 0, "negative seek target {to_ms}");
            }
        }

        // (b) |drift| within threshold and states agree -> always None.
        #[test]
        fn within_threshold_and_states_agree_is_none(
            base in -1_000_000_000i64..1_000_000_000,
            playing in any::<bool>(),
            threshold in 0u64..10_000,
            offset in -10_000i64..10_000,
            now in arb_u64(),
            cooldown in arb_u64(),
            last in proptest::option::of(arb_u64()),
        ) {
            let drift = offset.clamp(-(threshold as i64), threshold as i64);
            let t = Target { position_ms: base, playing };
            // Paused receiver: no interpolation, so drift is exactly the offset.
            let o = Observed { position_ms: base + drift, is_paused: !playing, age_ms: 0 };
            let cfg = PlannerConfig { threshold_ms: threshold, cooldown_ms: cooldown, seek_lead_ms: 0 };
            let mut st = PlannerState { last_correction_at_ms: last };
            prop_assert_eq!(plan(&t, &o, now, &cfg, &mut st), Action::None);
        }

        // (a) two non-None actions are never closer than cooldown_ms, except the
        // very first correction, which has no predecessor to be closer to.
        #[test]
        fn corrections_are_spaced_by_cooldown(
            steps in proptest::collection::vec(
                (arb_target(), arb_observed(), 0u64..20_000),
                1..40,
            ),
            cfg in arb_cfg(),
        ) {
            let mut st = PlannerState::default();
            let mut now: u64 = 0;
            let mut last_fired: Option<u64> = None;
            for (t, o, dt) in steps {
                now = now.saturating_add(dt);
                let action = plan(&t, &o, now, &cfg, &mut st);
                if action != Action::None {
                    if let Some(prev) = last_fired {
                        prop_assert!(
                            now - prev >= cfg.cooldown_ms,
                            "corrections {prev} and {now} closer than cooldown {}",
                            cfg.cooldown_ms
                        );
                    }
                    last_fired = Some(now);
                }
            }
        }
    }
}
