//! Read-only input inspection: editing and playback have separate boundaries.

use super::*;
use serde_json::{Value, json};

/// The current project renderer has one MIDI receiver. A declared port is
/// harmless when every routed event uses that same receiver, including a
/// nonzero port number. Never merge events from distinct receivers.
pub(super) fn routing_blockers(smf: &Smf<'_>) -> Vec<Diagnostic> {
    let mut receiver = None;
    let mut diagnostics = Vec::new();
    for (track, events) in smf.tracks.iter().enumerate() {
        let mut port = 0;
        for (index, event) in events.iter().enumerate() {
            match event.kind {
                TrackEventKind::Meta(MetaMessage::MidiPort(value)) => port = value.as_int(),
                TrackEventKind::Midi { .. }
                | TrackEventKind::SysEx(_)
                | TrackEventKind::Escape(_) => {
                    if let Some(previous) = receiver {
                        if port != previous {
                            diagnostics.push(Diagnostic {
                                code: Code::UnsupportedMidi,
                                location: format!("/tracks/{track}/events/{index}"),
                                message: format!("MIDI ports {previous} and {port} address different receivers; multi-port editing and rendering are not supported"),
                            });
                            return diagnostics;
                        }
                    } else {
                        receiver = Some(port);
                    }
                }
                _ => {}
            }
        }
    }
    diagnostics
}

fn gm_initialization(data: &[u8]) -> bool {
    matches!(data, [0x7e, 0x7f, 0x09, 0x01 | 0x03, 0xf7])
}

/// Messages the reference synthesizer cannot interpret must never disappear
/// silently from a preview. Metadata remains in the source but needs no sound.
pub fn preview_blockers(bytes: &[u8]) -> Vec<Diagnostic> {
    let Ok(smf) = Smf::parse(bytes) else {
        return vec![Diagnostic {
            code: Code::UnsupportedMidi,
            location: "/header".into(),
            message: "invalid Standard MIDI File".into(),
        }];
    };
    let mut diagnostics = routing_blockers(&smf);
    // A fresh reference receiver already has its initial state. GM1/GM2
    // initialization before any channel state needs no further sound action.
    // Resets later in the song must not silently lose their state changes.
    let first_channel = smf
        .tracks
        .iter()
        .enumerate()
        .filter_map(|(track, events)| {
            let mut tick = 0u64;
            events.iter().enumerate().find_map(|(index, event)| {
                tick += u64::from(event.delta.as_int());
                matches!(event.kind, TrackEventKind::Midi { .. }).then_some((tick, track, index))
            })
        })
        .min();
    for (track, events) in smf.tracks.iter().enumerate() {
        let mut tick = 0u64;
        for (index, event) in events.iter().enumerate() {
            tick += u64::from(event.delta.as_int());
            let unsupported = match event.kind {
                TrackEventKind::SysEx(data)
                    if gm_initialization(data)
                        && first_channel.is_none_or(|first| (tick, track, index) < first) =>
                {
                    false
                }
                TrackEventKind::SysEx(_) | TrackEventKind::Escape(_) => true,
                TrackEventKind::Midi {
                    message: MidiMessage::Aftertouch { .. },
                    ..
                } => true,
                // CC20..31 have no assigned GM sound function. They still
                // reach the SoundFont's controller modulators; custom game
                // meanings (such as loop cues) are not inferred.
                TrackEventKind::Midi {
                    message: MidiMessage::Controller { controller, .. },
                    ..
                } => !matches!(
                    controller.as_int(),
                    0 | 1 | 6 | 7 | 10 | 11 | 20..=31 | 38 | 64 | 66 | 91 | 93 | 98..=101 | 120 | 121 | 123..=127
                ),
                _ => false,
            };
            if unsupported {
                diagnostics.push(Diagnostic { code: Code::UnsupportedMidi, location: format!("/tracks/{track}/events/{index}"), message: "this event is preserved in MIDI but has no verified reference-synth interpretation; preview is blocked".into() });
            }
        }
    }
    diagnostics
}

pub fn analyze_smf(bytes: &[u8]) -> Value {
    let mut controllers: BTreeMap<u8, (usize, BTreeMap<u8, usize>)> = BTreeMap::new();
    let mut header = Value::Null;
    if let Ok(smf) = Smf::parse(bytes) {
        header = json!({"format": format!("{:?}", smf.header.format), "timing": format!("{:?}", smf.header.timing), "tracks": smf.tracks.len()});
        for event in smf.tracks.iter().flatten() {
            if let TrackEventKind::Midi {
                message: MidiMessage::Controller { controller, value },
                ..
            } = event.kind
            {
                let entry = controllers.entry(controller.as_int()).or_default();
                entry.0 += 1;
                *entry.1.entry(value.as_int()).or_default() += 1;
            }
        }
    }
    let result = read_smf_preserving(bytes);
    let diagnostics = result.as_ref().err().cloned().unwrap_or_default();
    let preview = preview_blockers(bytes);
    let standalone = super::source::source_summary(bytes).is_ok();
    let embedded = if standalone {
        vec![]
    } else {
        super::source::embedded_midi(bytes)
    };
    json!({
        "source_sha256": crate::store::sha256_hex(bytes), "bytes": bytes.len(), "header": header,
        "can_load": standalone || !embedded.is_empty(),
        "embedded_sources": embedded,
        "can_import": result.is_ok(),
        "controllers": controllers.into_iter().map(|(cc, (count, values))| json!({"controller": cc, "events": count, "values": values})).collect::<Vec<_>>(),
        "diagnostic_count": diagnostics.len(), "diagnostics": diagnostics.iter().take(50).collect::<Vec<_>>(),
        "editing": {
            "unchanged_export": "exact original bytes, including encoding and event order",
            "supported": ["notes within existing voices", "program, volume, pan, expression, modulation, reverb, chorus and pitch-bend lanes", "tempo and meter", "project roles and sections"],
            "retained_only": "unmodeled events, original track identities, markers, loop/end boundaries and RPN range events",
            "checks": "each edit is serialized and read back before commit; unsupported changes leave the revision unchanged",
            "edited_export": "unrelated events retain payload, absolute ticks, track and relative order; byte encoding may change",
            "retained": result.as_ref().ok().map(|r| &r.retained),
            "projection_normalizations": result.as_ref().ok().map(|r| &r.normalizations)
        },
        "preview": {"available": result.is_ok() && preview.is_empty(), "blocker_count": preview.len(), "diagnostics": preview.iter().take(20).collect::<Vec<_>>(), "scope": "reference sound only; MIDI preservation does not establish original-device or target audio equivalence"},
        "content": result.as_ref().ok().map(|r| json!({"timebase": r.timebase, "voices": r.composition.voices.len(), "notes": r.composition.notes.len()})),
        "next_action": if result.is_ok() { "Load with load_midi and inspect original events with inspect_midi. Optionally import the loaded source for editing, then export. Use compare_midi to verify unchanged delivery; an intentional edit should produce differences." } else { "Use load_midi and inspect_midi to retain and inspect the original without a composition or mapping. These diagnostics concern the optional editable projection. Do not strip events to make the projection pass." },
    })
}
