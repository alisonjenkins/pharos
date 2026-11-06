//! The effect plan: one [`LineEffect`] per source line (spec 028 FR-001).

use serde::{Deserialize, Serialize};

/// What kind of speech a line is. Sarcasm is never picked by the phase 1
/// director (it needs a model) but the type and compiler carry it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    Plain,
    Shout,
    Whisper,
    Thought,
    Narration,
    OffScreen,
    Phone,
    AttackCall,
    Flashback,
    Sarcasm,
    Sfx,
}

impl Intent {
    /// Every intent, in declaration order.
    pub const ALL: [Intent; 11] = [
        Intent::Plain,
        Intent::Shout,
        Intent::Whisper,
        Intent::Thought,
        Intent::Narration,
        Intent::OffScreen,
        Intent::Phone,
        Intent::AttackCall,
        Intent::Flashback,
        Intent::Sarcasm,
        Intent::Sfx,
    ];

    /// Stable snake_case name, identical to the serde form.
    pub fn label(self) -> &'static str {
        match self {
            Intent::Plain => "plain",
            Intent::Shout => "shout",
            Intent::Whisper => "whisper",
            Intent::Thought => "thought",
            Intent::Narration => "narration",
            Intent::OffScreen => "off_screen",
            Intent::Phone => "phone",
            Intent::AttackCall => "attack_call",
            Intent::Flashback => "flashback",
            Intent::Sarcasm => "sarcasm",
            Intent::Sfx => "sfx",
        }
    }
}

/// Where a line is drawn. `Free` is in script units (PlayRes 1920x1080).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    #[default]
    Default,
    Top,
    Free {
        x: i32,
        y: i32,
    },
}

/// Karaoke fill style, applied when the source line carries syllable timings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Karaoke {
    #[default]
    None,
    Sweep,
    Bounce,
    Ember,
}

/// The director's decision for one source line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineEffect {
    /// Index into the source lines this plan was directed from.
    pub line: usize,
    pub intent: Intent,
    /// 0 (no effect) to 3 (loudest).
    pub intensity: u8,
    #[serde(default)]
    pub placement: Placement,
    #[serde(default)]
    pub karaoke: Karaoke,
    /// The rule and inputs that chose this, for logs.
    #[serde(default)]
    pub reason: String,
    /// The line crosses a scene cut: keep the intent but draw the Low (still)
    /// form, so no animation runs across the cut (FR-011).
    #[serde(default)]
    pub static_only: bool,
}

/// A directed track: one entry per line that is not left alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectPlan {
    pub director_version: u32,
    pub lines: Vec<LineEffect>,
}

/// Device budget class (spec 028 FR-006).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Low,
    Tv,
    Desktop,
}

impl Tier {
    pub const ALL: [Tier; 3] = [Tier::Low, Tier::Tv, Tier::Desktop];

    pub fn label(self) -> &'static str {
        match self {
            Tier::Low => "low",
            Tier::Tv => "tv",
            Tier::Desktop => "desktop",
        }
    }

    pub fn parse(s: &str) -> Option<Tier> {
        Tier::ALL.into_iter().find(|t| t.label() == s)
    }

    /// The next cheaper tier; `Low` is the floor.
    pub fn down(self) -> Tier {
        match self {
            Tier::Desktop => Tier::Tv,
            Tier::Tv | Tier::Low => Tier::Low,
        }
    }
}

/// The viewer's Effects setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectsLevel {
    Off,
    Subtle,
    Full,
}

impl EffectsLevel {
    pub const ALL: [EffectsLevel; 3] =
        [EffectsLevel::Off, EffectsLevel::Subtle, EffectsLevel::Full];

    pub fn label(self) -> &'static str {
        match self {
            EffectsLevel::Off => "off",
            EffectsLevel::Subtle => "subtle",
            EffectsLevel::Full => "full",
        }
    }

    pub fn parse(s: &str) -> Option<EffectsLevel> {
        EffectsLevel::ALL.into_iter().find(|l| l.label() == s)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn intent_labels_are_distinct_and_match_serde() {
        let labels: BTreeSet<_> = Intent::ALL.iter().map(|i| i.label()).collect();
        assert_eq!(labels.len(), Intent::ALL.len());
        for i in Intent::ALL {
            let json = serde_json::to_string(&i).unwrap();
            assert_eq!(json, format!("\"{}\"", i.label()));
        }
    }

    #[test]
    fn tier_and_level_labels_round_trip() {
        for t in Tier::ALL {
            assert_eq!(Tier::parse(t.label()), Some(t));
        }
        for l in EffectsLevel::ALL {
            assert_eq!(EffectsLevel::parse(l.label()), Some(l));
        }
        assert_eq!(Tier::parse("huge"), None);
        assert_eq!(EffectsLevel::parse(""), None);
    }

    #[test]
    fn tier_down_stops_at_low() {
        assert_eq!(Tier::Desktop.down(), Tier::Tv);
        assert_eq!(Tier::Tv.down(), Tier::Low);
        assert_eq!(Tier::Low.down(), Tier::Low);
    }

    #[test]
    fn plan_round_trips_through_json() {
        let plan = EffectPlan {
            director_version: 1,
            lines: vec![
                LineEffect {
                    line: 0,
                    intent: Intent::Shout,
                    intensity: 3,
                    placement: Placement::Free { x: 960, y: 200 },
                    karaoke: Karaoke::None,
                    reason: "loud".into(),
                    static_only: false,
                },
                LineEffect {
                    line: 4,
                    intent: Intent::Plain,
                    intensity: 0,
                    placement: Placement::Top,
                    karaoke: Karaoke::Ember,
                    reason: String::new(),
                    static_only: false,
                },
            ],
        };
        let json = serde_json::to_string(&plan).unwrap();
        let back: EffectPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan, back);
    }

    #[test]
    fn plan_defaults_missing_optional_fields() {
        let json =
            r#"{"director_version":1,"lines":[{"line":2,"intent":"whisper","intensity":1}]}"#;
        let plan: EffectPlan = serde_json::from_str(json).unwrap();
        assert_eq!(plan.lines[0].placement, Placement::Default);
        assert_eq!(plan.lines[0].karaoke, Karaoke::None);
    }
}
