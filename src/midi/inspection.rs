//! Read-only input inspection: editing and playback have separate boundaries.

use super::*;
use serde_json::{Value, json};

/// Describe retained routing without merging distinct receivers.
pub(super) fn port_routing_note(smf: &Smf<'_>) -> String {
    let mut declared = std::collections::BTreeSet::new();
    let mut carrying = std::collections::BTreeSet::new();
    for events in &smf.tracks {
        let mut port = 0;
        for event in events {
            match event.kind {
                TrackEventKind::Meta(MetaMessage::MidiPort(value)) => {
                    port = value.as_int();
                    declared.insert(port);
                }
                TrackEventKind::Midi { .. }
                | TrackEventKind::SysEx(_)
                | TrackEventKind::Escape(_) => {
                    carrying.insert(port);
                }
                _ => {}
            }
        }
    }
    let list = |ports: &std::collections::BTreeSet<u8>| {
        let names: Vec<String> = ports.iter().map(u8::to_string).collect();
        match names.len() {
            1 => format!("port {}", names[0]),
            _ => format!("ports {}", names.join(" and ")),
        }
    };
    let carried = match carrying.iter().next() {
        Some(_) if carrying.len() > 1 => {
            format!("{} carry events on independent receivers", list(&carrying))
        }
        None => "no channel or SysEx events are routed".to_owned(),
        Some(port) if declared.len() > 1 || !declared.contains(port) => {
            format!(
                "only port {port} carries channel and SysEx events, the receiver the projection uses"
            )
        }
        Some(port) => format!("every channel and SysEx event uses port {port}"),
    };
    format!(
        "MIDI Port event retained unchanged; the file declares {} and {carried}",
        list(&declared)
    )
}

/// A broadcast GM1 or GM2 System On message.
pub fn gm_initialization(data: &[u8]) -> bool {
    matches!(data, [0x7e, 0x7f, 0x09, 0x01 | 0x03, 0xf7])
}

/// Channel messages the reference receiver interprets. Poly pressure and
/// controllers without an implemented meaning return false.
pub fn reference_channel_message(message: MidiMessage) -> bool {
    match message {
        MidiMessage::Aftertouch { .. } => false,
        // CC20..31 have no assigned GM sound function. They still reach the
        // SoundFont's controller modulators; custom game meanings (such as
        // loop cues) are not inferred.
        MidiMessage::Controller { controller, .. } => matches!(
            controller.as_int(),
            0 | 1 | 6 | 7 | 10 | 11 | 20..=31 | 38 | 64 | 66 | 91 | 93 | 98..=101 | 120 | 121 | 123..=127
        ),
        _ => true,
    }
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
    crate::render::project_preview_blockers(&smf)
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
    let source = crate::render::midi_source_playback(bytes);
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
            "retained_only": "unmodeled events, note events without an editable note (releases without a sounding note, notes never released, zero-length pairs), original track identities, markers, loop/end boundaries and RPN range events",
            "note_pairing": "per track, port, channel and key; a release ends the earliest sounding onset (first in, first out). Stacked onsets stay separate notes bound to their own source events; edits that create new stacks or change the pairing are refused",
            "shared_channels": "a control lane stays with the track holding its events; changes to it, and note changes on a key another voice sounds on the same port and channel, must be in scope for and are protected by every voice on that channel",
            "checks": "each edit is serialized and read back before commit; unsupported changes leave the revision unchanged",
            "edited_export": "unrelated events retain payload, absolute ticks, track and relative order; byte encoding may change",
            "retained": result.as_ref().ok().map(|r| &r.retained),
            "projection_normalizations": result.as_ref().ok().map(|r| &r.normalizations)
        },
        "preview": {"available": source["available"], "project_available": result.is_ok() && preview.is_empty(), "source_problem": source.get("problem"), "timed_content": source.get("timed_content"), "source_diagnostics": source.get("diagnostics"), "receivers": source.get("receivers"), "sequence_count": source.get("sequence_count"), "sequences": source.get("sequences"), "route": "render_preview with source, destination and layout (and sequence for type 2); full-pass preflight at 44100 Hz", "blocker_count": preview.len(), "diagnostics": preview.iter().take(20).collect::<Vec<_>>(), "diagnostics_scope": "blocker_count and diagnostics concern project preview; source_diagnostics are what source-only render_preview returns", "scope": "reference sound only; MIDI preservation does not establish original-device or target audio equivalence"},
        "content": result.as_ref().ok().map(|r| json!({"timebase": r.timebase, "voices": r.composition.voices.len(), "notes": r.composition.notes.len()})),
        "next_action": if result.is_ok() { "Load with load_midi and inspect original events with inspect_midi. Optionally import the loaded source for editing, then export. Use compare_midi to verify unchanged delivery; an intentional edit should produce differences." } else { "Use load_midi and inspect_midi to retain and inspect the original without a composition or mapping. These diagnostics concern the optional editable projection. Check preview for source-only render_preview playback. Do not strip events to make the projection pass." },
    })
}
