//! Hand-off to external frame converter for a sound chip target.
//!
//! The adapter turns a revision into the converter's note timeline with an
//! explicit voice → chip, patch and pan mapping, then runs the converter as a
//! separate process. Chip meaning, frame conversion and rendering stay owned
//! by the converter; everything MIDI-level that does not carry over is
//! reported as a loss.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::composition::{Composition, ControlKind, Timebase};
use crate::diagnostic::{Diagnostic, DiagnosticCode as Code};
use crate::store::sha256_hex;
use crate::timeline::{Rational, seconds_at};

/// How voices become chip voices. Patch data comes from the consumer project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetMapping {
    /// Driver update rate in frames per second, such as 60.
    pub frames_per_second: u32,
    pub voices: Vec<ChipVoice>,
    /// External converter `frame_patch` objects by patch name.
    pub patches: BTreeMap<String, Value>,
    /// Voices left out on purpose; each is reported as a loss.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub omit: Vec<String>,
    /// Round note boundaries to the nearest frame instead of blocking; each
    /// moved boundary is reported.
    #[serde(default)]
    pub quantize: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChipVoice {
    pub voice: String,
    /// External converter chip name, such as `ym2612` or `sn76489`.
    pub chip: String,
    pub patch: String,
    /// External converter pan value; YM2612 uses 64, 128 or 192.
    pub pan: u8,
    /// External converter role label; defaults to the voice's role or id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct TargetTimeline {
    pub timeline: Value,
    pub voice_map: Value,
    pub losses: Vec<Value>,
}

/// Builds the converter timeline. Blocks instead of guessing when a voice is
/// unmapped, a note is off the frame grid without `quantize`, or a mapped
/// voice plays two notes at once.
pub fn build_timeline(
    title: &str,
    source: (&str, &str),
    timebase: Timebase,
    composition: &Composition,
    mapping: &TargetMapping,
) -> Result<TargetTimeline, Vec<Diagnostic>> {
    let mut diagnostics = Vec::new();
    let mut losses = Vec::new();
    let refuse = |diagnostics: &mut Vec<Diagnostic>, location: String, message: String| {
        diagnostics.push(Diagnostic {
            code: Code::InvalidRange,
            location,
            message,
        })
    };
    if mapping.frames_per_second == 0 {
        refuse(
            &mut diagnostics,
            "/mapping/frames_per_second".into(),
            "the frame rate must be positive".into(),
        );
        return Err(diagnostics);
    }
    let rate = i128::from(mapping.frames_per_second);
    let frame_of = |position: u64| {
        seconds_at(
            timebase,
            composition,
            Rational::from_integer(position.into()),
        ) * rate
    };

    if !composition.mix.is_empty() {
        losses.push(json!({"kind": "mix_not_transferred", "items": crate::mix::mix_changes(&Default::default(), &composition.mix).into_iter().map(|(_, label)| label).collect::<Vec<_>>()}));
    }
    for voice in &composition.voices {
        let mapped = mapping
            .voices
            .iter()
            .filter(|v| v.voice == voice.id)
            .count();
        let omitted = mapping.omit.contains(&voice.id);
        match (mapped, omitted) {
            (0, false) => refuse(
                &mut diagnostics,
                "/mapping/voices".into(),
                format!(
                    "voice `{}` has no chip voice; map it or list it in omit",
                    voice.id
                ),
            ),
            (0, true) => losses.push(json!({"kind": "voice_omitted", "voice": voice.id})),
            (1, false) => {}
            _ => refuse(
                &mut diagnostics,
                "/mapping/voices".into(),
                format!(
                    "voice `{}` is mapped more than once or both mapped and omitted",
                    voice.id
                ),
            ),
        }
    }
    for (i, chip_voice) in mapping.voices.iter().enumerate() {
        if !composition.voices.iter().any(|v| v.id == chip_voice.voice) {
            refuse(
                &mut diagnostics,
                format!("/mapping/voices/{i}/voice"),
                format!("voice `{}` does not exist", chip_voice.voice),
            );
        }
        match mapping.patches.get(&chip_voice.patch) {
            None => refuse(
                &mut diagnostics,
                format!("/mapping/voices/{i}/patch"),
                format!("patch `{}` is not defined", chip_voice.patch),
            ),
            Some(patch)
                if patch.get("chip").and_then(Value::as_str) != Some(chip_voice.chip.as_str()) =>
            {
                refuse(
                    &mut diagnostics,
                    format!("/mapping/patches/{}", chip_voice.patch),
                    format!(
                        "patch `{}` is not a `{}` patch",
                        chip_voice.patch, chip_voice.chip
                    ),
                )
            }
            Some(_) => {}
        }
    }

    let end_frames = frame_of(composition.playback.end);
    let end_tick = if end_frames.is_integer() || mapping.quantize {
        if !end_frames.is_integer() {
            losses.push(json!({"kind": "time_quantized", "what": "playback end", "exact_frames": rational_text(end_frames)}));
        }
        round_half_up(end_frames)
    } else {
        refuse(
            &mut diagnostics,
            "/playback/end".into(),
            format!(
                "the end falls at {} frames; set quantize or move it onto the frame grid",
                rational_text(end_frames)
            ),
        );
        0
    };

    let mut tracks = Vec::new();
    for chip_voice in &mapping.voices {
        let mut notes: Vec<(i128, i128, &crate::composition::Note)> = Vec::new();
        for note in composition
            .notes
            .iter()
            .filter(|n| n.voice == chip_voice.voice)
        {
            let mut edges = [frame_of(note.start), frame_of(note.start + note.duration)];
            for (edge, name) in edges.iter_mut().zip(["start", "end"]) {
                if !edge.is_integer() {
                    if mapping.quantize {
                        losses.push(
                            json!({"kind": "time_quantized", "note": note.id, "edge": name,
                            "exact_frames": rational_text(*edge), "frame": round_half_up(*edge)}),
                        );
                        *edge = Rational::from_integer(round_half_up(*edge));
                    } else {
                        refuse(
                            &mut diagnostics,
                            "/notes".into(),
                            format!(
                                "the {name} of `{}` falls at {} frames; set quantize or move it onto the grid",
                                note.id,
                                rational_text(*edge)
                            ),
                        );
                    }
                }
            }
            let (start, end) = (edges[0].to_integer(), edges[1].to_integer());
            if end <= start {
                refuse(
                    &mut diagnostics,
                    "/notes".into(),
                    format!("`{}` is shorter than one frame", note.id),
                );
                continue;
            }
            notes.push((start, end, note));
        }
        notes.sort_by_key(|(start, _, note)| (*start, note.id.clone()));
        for pair in notes.windows(2) {
            if pair[1].0 < pair[0].1 {
                refuse(
                    &mut diagnostics,
                    "/notes".into(),
                    format!(
                        "`{}` and `{}` overlap in `{}`; a chip voice plays one note at a time",
                        pair[0].2.id, pair[1].2.id, chip_voice.voice
                    ),
                );
            }
        }
        for lane in composition
            .control_lanes
            .iter()
            .filter(|l| l.voice == chip_voice.voice)
        {
            let kind = match lane.kind {
                ControlKind::Program => "program_replaced_by_patch_mapping",
                ControlKind::PitchBend if lane.points.iter().all(|p| p.value == 0) => continue,
                ControlKind::PitchBendRange => continue,
                ControlKind::PitchBend => "pitch_bend_not_transferred",
                ControlKind::Volume | ControlKind::Expression => "level_control_not_transferred",
                ControlKind::Pan => "pan_replaced_by_mapping",
                ControlKind::Modulation | ControlKind::Reverb | ControlKind::Chorus => {
                    "effect_control_not_transferred"
                }
            };
            losses.push(
                json!({"kind": kind, "voice": chip_voice.voice, "points": lane.points.len()}),
            );
        }
        tracks.push(json!({
            "id": chip_voice.voice,
            "notes": notes.iter().map(|(start, end, note)| json!({
                "start": start, "duration": end - start, "pitch_cents": u32::from(note.pitch) * 100,
                "velocity": note.velocity, "instrument": chip_voice.patch,
            })).collect::<Vec<_>>(),
        }));
    }
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    let loop_frames = match composition.playback.loop_start {
        Some(start) => {
            let frames = frame_of(start);
            if !frames.is_integer() {
                losses.push(json!({"kind": "time_quantized", "what": "loop start", "exact_frames": rational_text(frames)}));
            }
            json!([round_half_up(frames), end_tick])
        }
        None => Value::Null,
    };
    let used: Vec<&String> = mapping.voices.iter().map(|v| &v.patch).collect();
    let instruments: BTreeMap<&String, Value> = mapping
        .patches
        .iter()
        .filter(|(name, _)| used.contains(name))
        .map(|(name, patch)| {
            (
                name,
                json!({"label": name, "kind": "tone", "source_parameters": {"frame_patch": patch}}),
            )
        })
        .collect();
    let timeline = json!({
        "title": title,
        "tick_rate": [mapping.frames_per_second, 1],
        "end_tick": end_tick,
        "loop": loop_frames,
        "instruments": instruments,
        "tracks": tracks,
        "sources": [{"name": source.0, "size": source.1.len(), "sha256": sha256_hex(source.1.as_bytes()), "locator": format!("tunemend:{}", source.0)}],
        "raw_events": [],
    });
    let voice_map: BTreeMap<&String, Value> = mapping
        .voices
        .iter()
        .map(|v| {
            let role = v
                .role
                .clone()
                .or_else(|| {
                    composition
                        .voices
                        .iter()
                        .find(|x| x.id == v.voice)
                        .and_then(|x| x.role.clone())
                })
                .unwrap_or_else(|| v.voice.clone());
            (
                &v.voice,
                json!({"role": role, "chip": v.chip, "patch": v.patch, "pan": v.pan}),
            )
        })
        .collect();
    Ok(TargetTimeline {
        timeline,
        voice_map: serde_json::to_value(voice_map).expect("serializes"),
        losses,
    })
}

fn round_half_up(value: Rational) -> i128 {
    (value + Rational::new(1, 2)).floor().to_integer()
}

fn rational_text(value: Rational) -> String {
    if value.is_integer() {
        value.to_integer().to_string()
    } else {
        format!("{}/{}", value.numer(), value.denom())
    }
}

/// One external step and what it left behind.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct StepResult {
    pub step: String,
    /// 0 success, 2 refused by the converter's checks, other values are failures.
    pub exit_code: i32,
    pub output: String,
    pub stderr: String,
    /// The `reason` the converter wrote into the step's report when it refused.
    pub reason: Option<String>,
}

/// Runs the converter executable: frame conversion, then capability check,
/// render and MIDI export of the frames. `destination` must be a new folder
/// under the workspace's `out/` or `work/`, which the converter requires.
pub fn run_converter(
    workspace: &Path,
    converter: &Path,
    timeline: &TargetTimeline,
    capability_profile: Option<&Path>,
    destination: &Path,
) -> std::io::Result<Vec<StepResult>> {
    std::fs::create_dir_all(destination)?;
    let timeline_path = destination.join("timeline.json");
    let voice_map_path = destination.join("voice-map.json");
    std::fs::write(
        &timeline_path,
        serde_json::to_vec_pretty(&timeline.timeline)?,
    )?;
    std::fs::write(
        &voice_map_path,
        serde_json::to_vec_pretty(&timeline.voice_map)?,
    )?;
    let run = |step: &str, args: Vec<PathBuf>, output: PathBuf| -> std::io::Result<StepResult> {
        let done = Command::new(converter)
            .arg(step)
            .args(&args)
            .arg("--out")
            .arg(&output)
            .current_dir(workspace)
            .output()?;
        let reason = std::fs::read_to_string(output.join("report.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|report| {
                report
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        Ok(StepResult {
            step: step.into(),
            exit_code: done.status.code().unwrap_or(-1),
            output: output.display().to_string(),
            stderr: String::from_utf8_lossy(&done.stderr)
                .trim()
                .chars()
                .take(2000)
                .collect(),
            reason,
        })
    };
    let frames_out = destination.join("frames");
    let frames_path = frames_out.join("frames.json");
    let mut steps = vec![run(
        "timeline-to-frames",
        vec![timeline_path, "--voice-map".into(), voice_map_path],
        frames_out,
    )?];
    if steps[0].exit_code != 0 {
        return Ok(steps);
    }
    if let Some(profile) = capability_profile {
        steps.push(run(
            "check-capability",
            vec![frames_path.clone(), "--profile".into(), profile.into()],
            destination.join("capability"),
        )?);
    }
    steps.push(run(
        "render",
        vec![frames_path.clone(), "--stems".into()],
        destination.join("render"),
    )?);
    steps.push(run(
        "export-midi",
        vec![frames_path],
        destination.join("midi"),
    )?);
    Ok(steps)
}

/// The Git checkout the converter executable was built in, if any: its
/// commit and whether the working tree had uncommitted changes.
pub fn converter_revision(converter: &Path) -> Option<Value> {
    let git = |args: &[&str]| -> Option<String> {
        let done = Command::new("git")
            .arg("-C")
            .arg(converter.parent()?)
            .args(args)
            .output()
            .ok()?;
        done.status
            .success()
            .then(|| String::from_utf8_lossy(&done.stdout).trim().to_owned())
    };
    let commit = git(&["rev-parse", "HEAD"])?;
    let changed = git(&["status", "--porcelain"]).map(|status| !status.is_empty());
    Some(json!({"commit": commit, "uncommitted_changes": changed}))
}
