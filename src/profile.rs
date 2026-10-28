//! Declared target constraints and the violations a composition has against them.
//!
//! A profile describes limits, not a sound chip: passing one says nothing
//! about how a particular game driver plays the result.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::composition::{Composition, Note, Timebase};
use crate::timeline::{Rational, seconds_at};

/// S-DSP voices of the SNES sound hardware.
pub const SNES_DSP_VOICES: u32 = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConstraintProfile {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Notes that may sound at once across all voices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_simultaneous_notes: Option<u32>,
    /// Notes kept free for sound effects; music must fit in the rest.
    #[serde(default)]
    pub reserved_for_effects: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_voices: Option<u32>,
    /// Rules per voice; a rule without `voice` applies to every voice.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub voice_rules: Vec<VoiceRule>,
    /// Every note start and end must fall on a multiple of 1/`per_second` s,
    /// such as a 60 Hz sound driver update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_grid_per_second: Option<u32>,
    /// The target needs exactly this many voices, such as a driver bank
    /// layout with fixed channels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_voices: Option<u32>,
    /// What looping the target can play back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_support: Option<LoopSupport>,
    /// Declared bound, in milliseconds, on how long a note of a voice with a
    /// chip instrument keeps sounding after its end, such as a driver's
    /// release cut. It replaces the release computed from the instrument's
    /// envelope when simultaneous notes are counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chip_release_ms: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LoopSupport {
    /// No repeat at all.
    None,
    /// Only the whole piece repeats: `loop_start` must be 0.
    Whole,
    /// Any single loop region.
    Region,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VoiceRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pitch_min: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pitch_max: Option<u8>,
    /// One note at a time in this voice.
    #[serde(default)]
    pub monophonic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ProfileReport {
    pub profile: String,
    pub passed: bool,
    pub checked: Vec<String>,
    /// Constraints the profile does not declare, so nothing was checked.
    pub unchecked: Vec<String>,
    pub violations: Vec<Violation>,
    /// With `max_simultaneous_notes`: the most notes counted at once, and
    /// where that count is first reached, whether or not it passes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_simultaneous_notes: Option<Peak>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Peak {
    pub count: usize,
    /// Position in the timebase unit of the start that first reaches it.
    pub at: u64,
    pub notes: Vec<String>,
}

/// Raises `peak` when `sounding` exceeds it after a note starts at `at`.
fn raise_peak(peak: &mut Option<Peak>, at: u64, sounding: &[&Note]) {
    if peak.as_ref().is_none_or(|p| sounding.len() > p.count) {
        let mut notes: Vec<&Note> = sounding.to_vec();
        notes.sort_by_key(|n| (n.start, n.id.clone()));
        *peak = Some(Peak {
            count: sounding.len(),
            at,
            notes: notes.iter().map(|n| n.id.clone()).collect(),
        });
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Violation {
    pub rule: String,
    /// Position in the timebase unit where the violation starts.
    pub at: u64,
    pub notes: Vec<String>,
    pub message: String,
}

/// Abstract profile for first tests: three music notes plus one reserved for
/// effects, monophonic voices in a mid range, timing on a 60 Hz grid.
pub fn test_profile() -> ConstraintProfile {
    ConstraintProfile {
        name: "test-four-channel".into(),
        description: Some("Abstract test limits; not a model of any console or driver.".into()),
        max_simultaneous_notes: Some(4),
        reserved_for_effects: 1,
        max_voices: Some(4),
        voice_rules: vec![VoiceRule {
            voice: None,
            pitch_min: Some(33),
            pitch_max: Some(96),
            monophonic: true,
        }],
        time_grid_per_second: Some(60),
        required_voices: None,
        loop_support: None,
        chip_release_ms: None,
    }
}

/// The SNES S-DSP voice budget: eight hardware voices, one per sounding note.
pub fn snes_dsp_profile() -> ConstraintProfile {
    ConstraintProfile {
        name: "snes-dsp-voices".into(),
        description: Some(
            "SNES S-DSP voice budget: at most 8 notes sound at once across all voices, each on one S-DSP voice; notes of voices with a chip instrument count until their release ends. A voice-count budget, not a sound-driver model: driver channel layout, voice stealing, sound-effect priority, timing and pitch limits are not checked. Keep voices free for sound effects with check_project reserved_for_effects.".into(),
        ),
        max_simultaneous_notes: Some(SNES_DSP_VOICES),
        reserved_for_effects: 0,
        max_voices: None,
        voice_rules: vec![],
        time_grid_per_second: None,
        required_voices: None,
        loop_support: None,
        chip_release_ms: None,
    }
}

pub fn builtin_profiles() -> Vec<ConstraintProfile> {
    vec![test_profile(), snes_dsp_profile()]
}

/// How long the notes of a voice with a chip instrument keep sounding after
/// their end: the instrument's release, computed or declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceRelease {
    pub seconds: Rational,
    /// How the bound was found, for the report.
    pub basis: String,
}

/// The release of each voice with a chip instrument, or why it is unknown.
pub type VoiceReleases = BTreeMap<String, Result<VoiceRelease, String>>;

pub fn check_profile(
    timebase: Timebase,
    composition: &Composition,
    profile: &ConstraintProfile,
    releases: &VoiceReleases,
) -> ProfileReport {
    let mut checked = Vec::new();
    let mut unchecked = Vec::new();
    let mut violations = Vec::new();
    let mut peak = None;

    if let Some(max) = profile.max_voices {
        checked.push(format!("at most {max} voices"));
        if composition.voices.len() as u32 > max {
            violations.push(Violation {
                rule: "max_voices".into(),
                at: 0,
                notes: vec![],
                message: format!(
                    "{} voices exceed the limit of {max}",
                    composition.voices.len()
                ),
            });
        }
    } else {
        unchecked.push("number of voices".into());
    }

    match profile.max_simultaneous_notes {
        Some(max) => {
            let budget = max.saturating_sub(if matches!(timebase, Timebase::Music { .. }) {
                profile.reserved_for_effects
            } else {
                0
            });
            checked.push(format!("at most {budget} simultaneous notes"));
            let known: BTreeMap<&str, &VoiceRelease> = releases
                .iter()
                .filter_map(|(voice, r)| r.as_ref().ok().map(|r| (voice.as_str(), r)))
                .filter(|(voice, _)| composition.notes.iter().any(|n| n.voice == *voice))
                .collect();
            if known.is_empty() {
                violations.extend(polyphony_violations(
                    &composition.notes,
                    budget,
                    "max_simultaneous_notes",
                    &mut peak,
                ));
                unchecked.push(
                    "release tails: no voice with notes has a chip instrument, so each note counts only until it ends; releases overlapping later notes, including across the loop return, are not checked".into(),
                );
            } else {
                checked.push(format!(
                    "notes of chip-instrument voices count until their release ends: {}",
                    known
                        .iter()
                        .map(|(voice, r)| format!(
                            "`{voice}` {:.2} ms after each note ({})",
                            1000.0 * *r.seconds.numer() as f64 / *r.seconds.denom() as f64,
                            r.basis
                        ))
                        .collect::<Vec<_>>()
                        .join("; ")
                ));
                violations.extend(sounding_violations(
                    timebase,
                    composition,
                    &known,
                    budget,
                    &mut peak,
                ));
                if composition
                    .voices
                    .iter()
                    .any(|v| !releases.contains_key(&v.id))
                {
                    unchecked.push(
                        "release tails of voices without a chip instrument; their notes count until they end".into(),
                    );
                }
                if composition.playback.loop_start.is_some() {
                    unchecked.push(
                        "chip release tails carried across the loop end into loop_start".into(),
                    );
                }
            }
            for (voice, problem) in releases
                .iter()
                .filter_map(|(v, r)| r.as_ref().err().map(|e| (v, e)))
            {
                unchecked.push(format!(
                    "the release of `{voice}`'s chip instrument ({problem}); its notes count until they end"
                ));
            }
        }
        None => unchecked.push("simultaneous notes".into()),
    }

    for rule in &profile.voice_rules {
        let applies = |note: &&Note| rule.voice.as_ref().is_none_or(|v| v == &note.voice);
        let target = rule.voice.as_deref().unwrap_or("every voice");
        if rule.pitch_min.is_some() || rule.pitch_max.is_some() {
            let (low, high) = (rule.pitch_min.unwrap_or(0), rule.pitch_max.unwrap_or(127));
            checked.push(format!("pitch {low}–{high} in {target}"));
            for note in composition
                .notes
                .iter()
                .filter(applies)
                .filter(|n| n.pitch < low || n.pitch > high)
            {
                violations.push(Violation {
                    rule: "pitch_range".into(),
                    at: note.start,
                    notes: vec![note.id.clone()],
                    message: format!(
                        "`{}` in `{}` has pitch {}, outside {low}–{high}",
                        note.id, note.voice, note.pitch
                    ),
                });
            }
        }
        if rule.monophonic {
            checked.push(format!("one note at a time in {target}"));
            let mut by_voice: BTreeMap<&str, Vec<Note>> = BTreeMap::new();
            for note in composition.notes.iter().filter(applies) {
                by_voice.entry(&note.voice).or_default().push(note.clone());
            }
            for notes in by_voice.values() {
                violations.extend(polyphony_violations(notes, 1, "monophonic", &mut None));
            }
        }
    }

    if let Some(required) = profile.required_voices {
        checked.push(format!("exactly {required} voices"));
        if composition.voices.len() as u32 != required {
            violations.push(Violation {
                rule: "required_voices".into(),
                at: 0,
                notes: vec![],
                message: format!(
                    "{} voices where the target needs exactly {required}",
                    composition.voices.len()
                ),
            });
        }
    }
    match (profile.loop_support, composition.playback.loop_start) {
        (None, _) => unchecked.push("loop playback on the target".into()),
        (Some(support), loop_start) => {
            checked.push(format!("loop playback: {support:?}"));
            let refused = match (support, loop_start) {
                (LoopSupport::None, Some(_)) => Some("the target cannot repeat"),
                (LoopSupport::Whole, Some(start)) if start != 0 => {
                    Some("the target repeats only the whole piece; set loop_start to 0")
                }
                _ => None,
            };
            if let Some(message) = refused {
                violations.push(Violation {
                    rule: "loop_support".into(),
                    at: loop_start.unwrap_or(0),
                    notes: vec![],
                    message: message.into(),
                });
            }
        }
    }

    match profile.time_grid_per_second {
        Some(rate) => {
            checked.push(format!("note boundaries on a 1/{rate} s grid"));
            for note in &composition.notes {
                for (edge, at) in [("start", note.start), ("end", note.start + note.duration)] {
                    let seconds =
                        seconds_at(timebase, composition, Rational::from_integer(at.into()));
                    let steps = seconds * i128::from(rate);
                    if !steps.is_integer() {
                        violations.push(Violation {
                            rule: "time_grid".into(),
                            at,
                            notes: vec![note.id.clone()],
                            message: format!(
                                "the {edge} of `{}` falls at {steps} steps of 1/{rate} s",
                                note.id
                            ),
                        });
                    }
                }
            }
        }
        None => unchecked.push("time resolution".into()),
    }
    unchecked
        .push("sound effect channel occupancy and return; no driver policy is declared".into());

    violations.sort_by(|a, b| (a.at, &a.rule).cmp(&(b.at, &b.rule)));
    ProfileReport {
        profile: profile.name.clone(),
        passed: violations.is_empty(),
        checked,
        unchecked,
        violations,
        peak_simultaneous_notes: peak,
    }
}

/// Like [`polyphony_violations`] over exact seconds, where notes of the
/// voices in `releases` keep sounding through their release. Within such a
/// voice, chip voices are allocated as renders allocate them: each note takes
/// the lowest-numbered chip voice whose previous note has ended, and a
/// release still sounding there is cut by the new key-on.
fn sounding_violations(
    timebase: Timebase,
    composition: &Composition,
    releases: &BTreeMap<&str, &VoiceRelease>,
    budget: u32,
    peak: &mut Option<Peak>,
) -> Vec<Violation> {
    let at = |tick: u64| seconds_at(timebase, composition, Rational::from_integer(tick.into()));
    // Each note's start, the end of its sound, and its own end.
    let mut spans: Vec<(Rational, Rational, Rational, &Note)> = Vec::new();
    let mut by_voice: BTreeMap<&str, Vec<&Note>> = BTreeMap::new();
    for note in &composition.notes {
        if releases.contains_key(note.voice.as_str()) {
            by_voice.entry(&note.voice).or_default().push(note);
        } else {
            let (start, end) = (at(note.start), at(note.start + note.duration));
            spans.push((start, end, end, note));
        }
    }
    for (voice, mut notes) in by_voice {
        let release = releases[voice].seconds;
        notes.sort_by_key(|n| (n.start, n.id.clone()));
        // Per chip voice: the key-off time of its note and that note's span.
        let mut chip_voices: Vec<(Rational, usize)> = Vec::new();
        for note in notes {
            let (start, end) = (at(note.start), at(note.start + note.duration));
            let index = spans.len();
            spans.push((start, end + release, end, note));
            match chip_voices.iter().position(|(off, _)| *off <= start) {
                Some(v) => {
                    let previous = chip_voices[v].1;
                    spans[previous].1 = spans[previous].1.min(start);
                    chip_voices[v] = (end, index);
                }
                None => chip_voices.push((end, index)),
            }
        }
    }
    let mut edges: Vec<(Rational, bool, usize)> = spans
        .iter()
        .enumerate()
        .flat_map(|(i, (start, end, _, _))| [(*start, true, i), (*end, false, i)])
        .collect();
    // Ends come first at the same time, so touching notes do not overlap.
    edges.sort_by_key(|&(t, starts, i)| (t, starts, i));
    let mut sounding: Vec<usize> = Vec::new();
    let mut violations = Vec::new();
    let mut over = false;
    for (t, starts, i) in edges {
        if starts {
            sounding.push(i);
            let notes: Vec<&Note> = sounding.iter().map(|&s| spans[s].3).collect();
            raise_peak(peak, spans[i].3.start, &notes);
        } else {
            sounding.retain(|&s| s != i);
        }
        let now_over = sounding.len() as u32 > budget;
        if now_over && !over {
            let releasing = sounding.iter().filter(|&&s| spans[s].2 <= t).count();
            let mut notes: Vec<&Note> = sounding.iter().map(|&s| spans[s].3).collect();
            notes.sort_by_key(|n| (n.start, n.id.clone()));
            violations.push(Violation {
                rule: "max_simultaneous_notes".into(),
                at: spans[i].3.start,
                notes: notes.iter().map(|n| n.id.clone()).collect(),
                message: format!(
                    "{} notes sound together where {budget} {} allowed{}",
                    sounding.len(),
                    if budget == 1 { "is" } else { "are" },
                    if releasing > 0 {
                        format!(", {releasing} of them in a chip-instrument release")
                    } else {
                        String::new()
                    }
                ),
            });
        }
        over = now_over;
    }
    violations
}

/// Reports each stretch where more than `budget` notes sound together.
fn polyphony_violations(
    notes: &[Note],
    budget: u32,
    rule: &str,
    peak: &mut Option<Peak>,
) -> Vec<Violation> {
    let mut edges: Vec<(u64, bool, &Note)> = notes
        .iter()
        .flat_map(|n| [(n.start, true, n), (n.start + n.duration, false, n)])
        .collect();
    // Releases come first at the same position, so touching notes do not overlap.
    edges.sort_by_key(|(at, starts, _)| (*at, *starts));
    let mut sounding: Vec<&Note> = Vec::new();
    let mut violations: Vec<Violation> = Vec::new();
    let mut over = false;
    for (at, starts, note) in edges {
        if starts {
            sounding.push(note);
            raise_peak(peak, at, &sounding);
        } else {
            sounding.retain(|n| n.id != note.id);
        }
        let now_over = sounding.len() as u32 > budget;
        if now_over && !over {
            violations.push(Violation {
                rule: rule.into(),
                at,
                notes: sounding.iter().map(|n| n.id.clone()).collect(),
                message: format!(
                    "{} notes sound together where {budget} {} allowed",
                    sounding.len(),
                    if budget == 1 { "is" } else { "are" }
                ),
            });
        }
        over = now_over;
    }
    violations
}
