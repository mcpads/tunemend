//! Read-only views: summaries, paged events, checks, comparisons and capabilities.

use super::*;

impl Service {
    pub(super) fn inspect(&mut self, input: InspectInput) -> Result<ToolResponse, ToolResponse> {
        if let InspectView::Projects = input.view {
            if !Store::exists(&self.workspace) {
                return Ok(ToolResponse::ok("no projects yet", json!({"projects": []})));
            }
            let store = self.store_ref()?;
            let reader = store.reader();
            let mut projects = Vec::new();
            for id in reader.project_ids()? {
                let (info, _) = reader.project(&id)?.expect("listed");
                projects.push(json!({"project": id, "timebase": info.timebase, "head_revision": info.head_revision,
                    "revisions": reader.revision_count(&id)?, "purpose": info.brief.map(|b| b.purpose)}));
            }
            return Ok(ToolResponse::ok(
                format!("{} projects", projects.len()),
                json!({"projects": projects}),
            ));
        }
        let project = input.project.clone().ok_or_else(|| {
            ToolResponse::refused(Code::InvalidRange, "/project", "this view needs a project")
        })?;
        match input.view {
            InspectView::Projects => unreachable!(),
            InspectView::Summary => {
                let (info, revision) = self.revision(&project, input.revision.as_deref())?;
                Ok(ToolResponse::ok(
                    format!("`{project}` {}", revision.id),
                    summary(&info, &revision),
                ))
            }
            InspectView::Events => {
                let (info, revision) = self.revision(&project, input.revision.as_deref())?;
                Ok(events_page(&info, &revision, &input))
            }
            InspectView::History => {
                let (info, _) = self.project(&project)?;
                let revisions = self.store_ref()?.reader().revisions(&project)?;
                let mut history = Vec::new();
                for revision in &revisions {
                    let parent = revision
                        .parent
                        .as_ref()
                        .and_then(|p| revisions.iter().find(|r| &r.id == p));
                    let difference = parent.map(|p| {
                        compare_compositions(&p.composition, &revision.composition, None, None)
                    });
                    history.push(json!({
                        "revision": revision.id, "parent": revision.parent,
                        "job_id": revision.edit_request.as_ref().map(|r| &r.job_id),
                        "rationale": revision.edit_request.as_ref().and_then(|r| r.rationale.as_ref()),

            "feedback": revision.edit_request.as_ref().map(|r| &r.feedback),
                        "changes": difference.map(|d| json!({"added_notes": d.added_notes.len(), "removed_notes": d.removed_notes.len(),
                            "changed_notes": d.changed_notes.len(), "changed_controls": d.changed_controls.len(), "changed_timeline": d.changed_timeline, "changed_mix": d.changed_mix})),
                    }));
                }
                Ok(ToolResponse::ok(
                    format!("{} revisions, head {}", history.len(), info.head_revision),
                    json!({"head_revision": info.head_revision, "history": history, "brief": info.brief}),
                ))
            }
            InspectView::Reviews => {
                let reader_store = self.store_ref()?;
                let reader = reader_store.reader();
                let renders: Vec<RenderRecord> = reader.documents(&project, "render")?;
                let presentations: Vec<Presentation> =
                    reader.documents(&project, "presentation")?;
                let feedback: Vec<Feedback> = reader.documents(&project, "feedback")?;
                let drafts: Vec<Feedback> = reader.documents(&project, DRAFTS)?;
                Ok(ToolResponse::ok(
                    format!(
                        "{} renders, {} presentations, {} delivered feedback",
                        renders.len(),
                        presentations.len(),
                        feedback.len()
                    ),
                    json!({
                        "renders": renders.iter().map(|r| {
                            if r.audio_present(reader_store) {
                                json!({"render": r.render, "audio": "present", "path": reader_store.object_path(&r.object)})
                            } else {
                                json!({"render": r.render, "audio": "absent",
                                    "restore": "render_batch regenerate_renders renders it again and checks its recorded sha256"})
                            }
                        }).collect::<Vec<_>>(),
                        "presentations": presentations,
                        "delivered": feedback.iter().map(|f| json!({"id": f.id, "state": f.state, "marks": f.marks.len(), "text": f.text})).collect::<Vec<_>>(),
                        "drafts_with_marks": drafts.iter().filter(|d| !d.marks.is_empty() || d.text.is_some()).count(),
                    }),
                ))
            }
        }
    }
    pub(super) fn check_project(
        &mut self,
        input: CheckProjectInput,
    ) -> Result<ToolResponse, ToolResponse> {
        let mut profile = match input.profile {
            ProfileChoice::Inline(profile) => profile,
            ProfileChoice::Named(name) => builtin_profiles()
                .into_iter()
                .find(|p| p.name == name)
                .ok_or_else(|| {
                ToolResponse::refused(
                    Code::UnknownReference,
                    "/profile",
                    format!(
                        "no built-in profile `{name}`; built-in profiles are {}",
                        builtin_profiles()
                            .iter()
                            .map(|p| p.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )
            })?,
        };
        if let Some(reserved) = input.reserved_for_effects {
            profile.reserved_for_effects = reserved;
        }
        let (info, revision) = self.revision(&input.project, input.revision.as_deref())?;
        let releases = if profile.max_simultaneous_notes.is_some() {
            self.voice_releases(&revision.composition.mix, profile.chip_release_ms)
        } else {
            Default::default()
        };
        let report = check_profile(info.timebase, &revision.composition, &profile, &releases);
        let summary = if report.passed {
            format!("{} passes `{}`", revision.id, profile.name)
        } else {
            format!(
                "{} has {} violations of `{}`",
                revision.id,
                report.violations.len(),
                profile.name
            )
        };
        Ok(ToolResponse::ok(
            summary,
            json!({"revision": revision.id, "report": report}),
        ))
    }
    pub(super) fn compare(&mut self, input: CompareInput) -> Result<ToolResponse, ToolResponse> {
        let (_, from) = self.revision(&input.project, Some(&input.from))?;
        let (_, to) = self.revision(&input.project, Some(&input.to))?;
        let difference = compare_compositions(
            &from.composition,
            &to.composition,
            input.range,
            input.voices.as_deref(),
        );
        // Every compared category appears, empty when nothing differs, so an
        // empty mixer or clip list reads as compared rather than skipped.
        let mut result = serde_json::to_value(&difference).expect("serializes");
        for category in ["changed_mix", "changed_clips"] {
            result
                .as_object_mut()
                .expect("a difference is an object")
                .entry(category)
                .or_insert_with(|| json!([]));
        }
        if input.voices.is_some() || input.range.is_some() {
            let notes = [
                "added_notes",
                "removed_notes",
                "changed_notes",
                "unchanged_notes",
                "changed_controls",
            ];
            let mut applies = serde_json::Map::new();
            if input.voices.is_some() {
                let mut voices: Vec<&str> = notes.to_vec();
                voices.extend(["changed_mix (items of the selected voices; bus and master items are always listed)", "changed_clips"]);
                applies.insert("voices".into(), json!(voices));
            }
            if input.range.is_some() {
                let mut range: Vec<&str> = notes.to_vec();
                range.push("changed_clips");
                applies.insert("range".into(), json!(range));
            }
            applies.insert(
                "never".into(),
                json!(["added_voices", "removed_voices", "changed_timeline"]),
            );
            if input.range.is_some() {
                applies.insert("not_by_range".into(), json!(["changed_mix"]));
            }
            result["filter_applies_to"] = Value::Object(applies);
        }
        Ok(ToolResponse::ok(
            format!(
                "{} → {}: {} added, {} removed, {} changed notes",
                from.id,
                to.id,
                difference.added_notes.len(),
                difference.removed_notes.len(),
                difference.changed_notes.len()
            ),
            result,
        ))
    }
}

pub(super) fn summary(info: &ProjectInfo, revision: &Revision) -> Value {
    let composition = &revision.composition;
    let voices: Vec<Value> = composition
        .voices
        .iter()
        .map(|voice| {
            let notes: Vec<&Note> = composition.notes.iter().filter(|n| n.voice == voice.id).collect();
            json!({
                "id": voice.id, "role": voice.role, "notes": notes.len(),
                "pitch_range": notes.iter().map(|n| n.pitch).min().zip(notes.iter().map(|n| n.pitch).max()),
                "first_start": notes.iter().map(|n| n.start).min(), "last_end": notes.iter().map(|n| n.start + n.duration).max(),
                "controls": composition.control_lanes.iter().filter(|l| l.voice == voice.id).map(|l| json!({"kind": l.kind, "points": l.points.len()})).collect::<Vec<_>>(),
            })
        })
        .collect();
    let end = seconds_at(
        info.timebase,
        composition,
        Rational::from_integer(composition.playback.end.into()),
    );
    let loop_seconds = composition.playback.loop_start.map(|start| {
        rational_seconds(seconds_at(
            info.timebase,
            composition,
            Rational::from_integer(start.into()),
        ))
    });
    json!({
        "project": info.name, "revision": revision.id, "head_revision": info.head_revision, "timebase": info.timebase,
        "unit": match info.timebase { Timebase::Music { .. } => "tick", Timebase::Effect => "microsecond" },
        "brief": info.brief, "midi_source": info.midi_source.as_ref().map(|source| json!({"sha256": source.sha256, "bytes": source.bytes.len(), "editing": crate::midi::analyze_smf(&source.bytes)["editing"]})),
        "playback": composition.playback, "duration_seconds": rational_seconds(end), "loop_start_seconds": loop_seconds,
        "tempo_map": composition.tempo_map, "time_signatures": composition.time_signatures, "key_signatures": composition.key_signatures,
        "sections": composition.sections, "markers": composition.markers, "voices": voices, "total_notes": composition.notes.len(),
        "mix": composition.mix.summary(),
    })
}

pub(super) fn events_page(
    info: &ProjectInfo,
    revision: &Revision,
    input: &InspectInput,
) -> ToolResponse {
    let composition = &revision.composition;
    let selected_voice = |voice: &str| {
        input
            .voices
            .as_ref()
            .is_none_or(|v| v.iter().any(|x| x == voice))
    };
    let in_range =
        |start: u64, end: u64| input.range.is_none_or(|r| start < r.end && end > r.start);
    let mut notes: Vec<&Note> = composition
        .notes
        .iter()
        .filter(|n| selected_voice(&n.voice) && in_range(n.start, n.start + n.duration))
        .collect();
    notes.sort_by(|a, b| (a.start, &a.voice, a.pitch).cmp(&(b.start, &b.voice, b.pitch)));
    let limit = input.limit.unwrap_or(200).clamp(1, MAX_PAGE) as usize;
    let offset = input.cursor.unwrap_or(0) as usize;
    let page: Vec<Value> = notes
        .iter()
        .skip(offset)
        .take(limit)
        .map(|n| {
            let mut value = serde_json::to_value(n).expect("serializes");
            let start = Rational::from_integer(n.start.into());
            value["position"] = json!(position_label(info.timebase, composition, start));
            if let Some(nearest) = nearest_label(info.timebase, composition, start) {
                value["nearest"] = json!(nearest);
            }
            value
        })
        .collect();
    let next = (offset + page.len() < notes.len()).then_some(offset + page.len());
    let controls: Vec<Value> = composition
        .control_lanes
        .iter()
        .filter(|l| selected_voice(&l.voice))
        .map(|l| {
            let points: Vec<_> = l
                .points
                .iter()
                .filter(|p| in_range(p.at, p.at + 1))
                .collect();
            json!({"voice": l.voice, "kind": l.kind, "points": points})
        })
        .collect();
    let mut response = ToolResponse::ok(
        format!("{} of {} notes from {}", page.len(), notes.len(), offset),
        json!({"revision": revision.id, "notes": page, "next_cursor": next, "controls": controls,
            "tempo_map": composition.tempo_map, "total_matching_notes": notes.len(),
            "mix_automation": composition.mix.automation.iter()
                .filter(|lane| match &lane.target { crate::mix::MixTarget::Voice { id } => selected_voice(id), _ => true })
                .map(|lane| json!({"target": lane.target, "parameter": lane.parameter,
                    "points": lane.points.iter().filter(|p| in_range(p.at, p.at + 1)).collect::<Vec<_>>()}))
                .collect::<Vec<_>>()}),
    );
    if !composition.mix.effect_automation.is_empty() {
        response.result["effect_automation"] = json!(
            composition
                .mix
                .effect_automation
                .iter()
                .filter(|lane| match &lane.target {
                    crate::mix::MixTarget::Voice { id } => selected_voice(id),
                    _ => true,
                })
                .map(|lane| json!({"target": lane.target, "effect": lane.effect,
                    "parameter": lane.parameter, "band": lane.band,
                    "points": lane.points.iter().filter(|p| in_range(p.at, p.at + 1)).collect::<Vec<_>>()}))
                .collect::<Vec<_>>()
        );
    }
    if !composition.mix.clips.is_empty() {
        response.result["clips"] = json!(
            composition
                .mix
                .clips
                .iter()
                .filter(
                    |c| selected_voice(&c.voice) && in_range(c.start, c.end().unwrap_or(u64::MAX))
                )
                .map(|c| {
                    let mut value = serde_json::to_value(c).expect("serializes");
                    let start = Rational::from_integer(c.start.into());
                    value["position"] = json!(position_label(info.timebase, composition, start));
                    if let Some(nearest) = nearest_label(info.timebase, composition, start) {
                        value["nearest"] = json!(nearest);
                    }
                    value
                })
                .collect::<Vec<_>>()
        );
    }
    response
}

/// One line per capability section; a refusal lists the names in this order.
const CAPABILITY_SECTIONS: &[(&str, &str)] = &[
    (
        "timebases",
        "music in integer ticks with PPQ and a tempo map; effects in microseconds",
    ),
    (
        "limits",
        "event page size, tail seconds, render voice and channel counts, sample rates",
    ),
    (
        "source_loading",
        "lossless MIDI loading (load_midi, inspect_midi, export_midi_source): formats, retained data, identity",
    ),
    (
        "midi_import",
        "what import_midi projects into an editable project, what blocks it and what is only retained",
    ),
    (
        "midi_export",
        "how export_project writes MIDI for new and imported projects",
    ),
    ("edit_operations", "every edit_project operation name"),
    (
        "mixer",
        "voice strips, buses, sends, volume/pan automation, effect chains, processors and effect automation shapes",
    ),
    (
        "voice_instruments",
        "chip instruments on voices (S-DSP samples, OPN FM): polyphony, rates, controls",
    ),
    (
        "audio_clips",
        "WAV clips placed on voices: sources, channels, rate, placement, fades, loops",
    ),
    (
        "bundles",
        "what export_project bundles carry and how import_project restores them",
    ),
    (
        "loops",
        "the loop region rule and what must match at loop_start",
    ),
    (
        "markers_and_regions",
        "sections (named regions) and markers, and what they need",
    ),
    (
        "render_batch",
        "deliverable spans, stems, stem_master, formats, records and regeneration",
    ),
    ("profiles", "built-in constraint profiles for check_project"),
    (
        "sound_sources",
        "the reference sound source render_preview uses",
    ),
    (
        "measurement",
        "measure_audio and compare_audio inputs, metrics, comparison, pitch and limits",
    ),
    ("feedback", "what listening marks mean"),
    (
        "sample_conversion",
        "prepare_sample, inspect_sample and export_sample: formats, codecs, resampling, loops, variants",
    ),
    (
        "instrument_banks",
        "instrument sources and banks: discovery, preservation, placement, delivery",
    ),
    (
        "instrument_audition",
        "render_instrument: the S-DSP voice model, inputs and what is modelled",
    ),
    (
        "fm_banks",
        "OPN FM banks: tools, layouts, preservation and render_fm_patch audition",
    ),
    (
        "vgm_mixing",
        "how render_psg mixes several chips of one VGM",
    ),
    (
        "vgm_transformation",
        "transform_vgm copies and SN76489-to-AY mapping, with loss codes",
    ),
    (
        "psg",
        "SN76489-family model: variants, clocks, VGM support, inputs, limits and model scope",
    ),
    (
        "ay8910",
        "AY-3-8910/YM2149 model: variants, clocks, VGM support, inputs and model scope",
    ),
    (
        "gb_dmg",
        "Game Boy DMG APU model: clocks, VGM support, inputs and model scope",
    ),
    (
        "nes_apu",
        "NES APU model: NTSC/PAL tables, VGM support, DMC, inputs, limits and model scope",
    ),
    (
        "huc6280",
        "HuC6280 PSG model: clocks, VGM support, inputs, limits and model scope",
    ),
    (
        "wonderswan",
        "WonderSwan sound unit model: clocks, VGM support, inputs, limits and model scope",
    ),
    (
        "observations",
        "attach_observation evidence: producer formats, receipts, limitations, subjects",
    ),
    (
        "target_conversion",
        "score target conversion (prepare_target, inspect_target, export_target) and convert_for_target",
    ),
];

/// Sections the summary returns whole because they are short and needed early.
const SUMMARY_SECTIONS: &[&str] = &["timebases", "limits"];

pub(super) fn capabilities(input: CapabilitiesInput) -> Handled {
    let full = full_capabilities();
    let Value::Object(all) = full else {
        unreachable!("capabilities are an object")
    };
    if input.detail == Some(CapabilityDetail::Full) {
        if input.sections.is_some() {
            return Err(ToolResponse::refused(
                Code::InvalidRange,
                "/sections",
                "detail full already returns every section; give sections or detail full, not both",
            ));
        }
        return done(ToolResponse::ok("Tunemend tools", Value::Object(all)));
    }
    let names = || {
        CAPABILITY_SECTIONS
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(", ")
    };
    if let Some(sections) = input.sections {
        if sections.is_empty() {
            return Err(ToolResponse::refused(
                Code::InvalidRange,
                "/sections",
                format!("name at least one section: {}", names()),
            ));
        }
        let mut result = serde_json::Map::new();
        result.insert("version".into(), all["version"].clone());
        for (i, name) in sections.iter().enumerate() {
            match all.get(name).filter(|_| name != "version") {
                Some(value) => {
                    result.insert(name.clone(), value.clone());
                }
                None => {
                    return Err(ToolResponse::refused(
                        Code::InvalidRange,
                        &format!("/sections/{i}"),
                        format!("no capability section `{name}`; sections: {}", names()),
                    ));
                }
            }
        }
        return done(ToolResponse::ok(
            format!("Tunemend capabilities: {}", sections.join(", ")),
            Value::Object(result),
        ));
    }
    let mut result = serde_json::Map::new();
    result.insert("version".into(), all["version"].clone());
    result.insert("detail".into(), json!("summary"));
    for name in SUMMARY_SECTIONS {
        result.insert((*name).into(), all[*name].clone());
    }
    result.insert(
        "sections".into(),
        Value::Object(
            CAPABILITY_SECTIONS
                .iter()
                .map(|(name, line)| ((*name).to_owned(), json!(line)))
                .collect(),
        ),
    );
    result.insert(
        "more".into(),
        json!("call get_capabilities with sections (for example [\"edit_operations\", \"mixer\"]) for those sections in full, or detail: full for every section"),
    );
    done(ToolResponse::ok(
        "Tunemend capabilities summary; request full sections with sections or detail: full",
        Value::Object(result),
    ))
}

/// Every capability section in full.
pub(super) fn full_capabilities() -> Value {
    // Two objects: one json! of every section exceeds the macro recursion limit.
    let mut capabilities = json!({
            "version": env!("CARGO_PKG_VERSION"),
            "timebases": {"music": "integer ticks with PPQ and a tempo map", "effect": "integer microseconds, no beat grid"},
            "source_loading": {
                "tools": ["load_midi", "inspect_midi", "export_midi_source"],
                "formats": ["SMF 0/1/2", "PPQ and SMPTE", "RIFF RMID", "structurally discovered embedded MIDI streams in opaque containers"],
                "data": "every channel event, metadata payload, SysEx, escape event, extension chunk and original byte is retained; no note pairing or target mapping is required",
                "identity": "immutable content hashes; containers retain all bytes and child offsets; duplicate streams retain distinct ranges",
                "inspection": "paged source events in original order, exact encoded byte ranges, and paged raw bytes",
                "projection": "import_midi is optional; a projection diagnostic does not mean source loading failed"
            },
            "midi_import": {
                "accepted": ["SMF type 0 and 1 with PPQ timing", "note on/off and note-on velocity 0", "tempo, time and key signatures", "markers, text, cue points, lyrics, copyright, track names", "program change, CC1 modulation, CC7 volume, CC10 pan, CC11 expression, CC91 reverb, CC93 chorus", "pitch bend and RPN 0 bend range in whole semitones"],
                "blocked": ["type 2 and SMPTE timing", "the same key restarting on a channel while it sounds", "unpaired or unreleased notes", "data entry outside whole-semitone RPN 0", "conflicting tempos across tracks or meters at one tick", "channel controls shared by several tracks", "events routed to multiple MIDI ports"],
                "retained_only": ["unmodeled control lanes, aftertouch and SysEx (preview support checked separately)", "single-port routing, channel prefix, external SMPTE synchronization offset and opaque metadata", "same-track same-tick controls and tempos (projection shows final value)", "terminal controls outside the editable timeline"],
                "inspection": "analyze_midi reports source identity, editable and retained data, import blockers and preview limits without creating a project",
                "preview": "single MIDI receiver; initial broadcast GM1/GM2 resets before channel setup, channel pressure and CC20..31 SoundFont modulators supported; custom game controller meanings and later SysEx resets are not inferred",
                "preservation": "original bytes, unknown events and same-tick order survive import/export; compare_midi verifies results",
                "assumed_and_reported": ["120 BPM when no tempo is at tick 0", "±2 semitone bend range before RPN 0"],
            },
            "midi_export": {"new_projects": "type 1; endings, controls, onsets; loops use loopStart/loopEnd markers", "imported_projects": "exact original bytes when unchanged; edits retain other event payloads, ticks, tracks and order", "verification": "compare_midi compares every decoded event class and byte identity; source edits are read back before commit"},
            "edit_operations": ["add_voice", "add_notes", "update_notes", "remove_notes", "shift_notes", "quantize_notes", "duplicate_range", "humanize_notes", "scale_velocities", "ramp_velocities", "legato_notes", "strum_notes", "snap_to_scale", "stretch_notes", "set_control_points", "ramp_control_points", "set_tempo_map", "set_time_signatures", "set_sections", "set_markers", "set_playback", "set_mix", "add_bus", "remove_bus", "set_send", "remove_send", "set_mix_automation", "set_effect_automation", "set_effects", "insert_effect", "remove_effect", "move_effect", "set_effect", "set_instrument", "clear_instrument", "add_clip", "update_clip", "remove_clip"],
            "mixer": {
                "data": "revisioned per-voice volume_db/pan/mute/solo/output, buses (volume_db, pan, mute) summing into the master, voice-to-bus sends (gain_db, pre_fader), master_volume_db, and step or linear volume/pan automation for voices, buses and the master (exponential applies to effect parameters on a ratio scale only)",
                "units": "dB with 0.1 dB resolution, -120.0 to 12.0; pan -100.0 (left) to 100.0 (right) as stereo balance with unity at the center; automation positions in the timebase unit",
                "scope": "voice strips, sends and voice automation are voice data under scope voices/range and protections; buses and the master need scope.buses",
                "render": "render_preview applies the mixer after the reference synthesizer, one synthesizer per voice; a revision without mixer data renders exactly as before",
                "midi": "not written to MIDI; the project bundle carries it and the export report lists it. Use volume/pan control lanes for MIDI CC7/CC10",
                "source_backed": "allowed as render and bundle state; source MIDI bytes never change for it",
                "effects": {
                    "chains": "one ordered insert chain (1-16 effects with id and bypass) per voice, bus or the master; voice and bus chains run before their fader and pan, the master chain after the master volume",
                    "processors": {
                        "eq": "RBJ cookbook biquads: peaking, low_shelf, high_shelf, low_pass, high_pass, band_pass, notch; frequency_hz, gain_db, q; frequencies above 0.45 x render rate are processed at that limit",
                        "compressor": "feed-forward, stereo-linked peak or 10 ms RMS detection; threshold_db, ratio, knee_db, attack_ms, release_ms, makeup_db; optional sidechain from another voice or a bus, acyclic",
                        "limiter": "sample-peak brickwall with zero-latency look-ahead (ceiling_db, release_ms, lookahead_ms); inter-sample peaks can exceed the ceiling",
                        "delay": "feedback delay in ms or a note fraction at the tempo map's tempo; feedback_percent, mix_percent, ping_pong",
                        "reverb": "Freeverb-style combs and allpasses scaled to the render rate; room_size_percent, damping_percent, pre_delay_ms, mix_percent",
                        "snes_echo": "S-DSP echo arithmetic: 8 FIR register bytes, EDL x 16 ms, EFB, EVOL left/right; runs at the render rate, so FIR responses scale with it unless the rate is 32000 Hz"
                    },
                    "deterministic": "offline f64 processing in fixed order; same build and platform give identical renders",
                    "tail": "with delay, reverb or echo the tail ends only after a silent run as long as their delay lines, within max_tail_seconds",
                    "automation": {
                        "operation": "set_effect_automation addresses target, effect id, parameter and, for eq gain_db and frequency_hz, a 0-based band; points like volume automation, with the effect's static setting before the first point",
                        "shapes": "step (hold), linear (in the parameter's unit) or exponential (geometric, equal ratios in equal time: EQ frequency_hz and compressor ratio only; both ends positive); exponential elsewhere refuses",
                        "parameters": {"eq": "gain_db -24..24 (peaking and shelf bands), frequency_hz 10..20000", "compressor": "threshold_db -60..0, ratio 1..100, makeup_db -24..24", "limiter": "ceiling_db -24..0", "delay": "feedback_percent 0..95, mix_percent 0..100", "reverb": "mix_percent, room_size_percent, damping_percent 0..100", "snes_echo": "feedback, volume_left, volume_right as whole numbers -128..127"},
                        "not_automatable": "delay time, pre-delay, look-ahead, attack, release, knee, q, filter kind, detection, EDL and FIR",
                        "evaluation": "every sample; equalizer coefficients are redesigned at each sample whose value differs from the previous one; register values round to the nearest whole number",
                        "removal": "a lane must be cleared in the request that removes its effect or parameter, or changes the effect's kind; move_effect keeps it; remove_bus removes the bus's lanes"
                    },
                    "midi": "not written to MIDI; listed in the export report"
                }
            },
            "voice_instruments": {
                "operations": ["set_instrument", "clear_instrument"],
                "data": "at most one per voice in the revision's mixer (mix.instruments); voice data under scope voices and protections, never limited by scope.range",
                "kinds": {
                    "snes_brr": "saved bank slot or BRR sample plan on the S-DSP voice model; pitch from an explicit root register and note, the plan's recorded root, or a fixed register; explicit ADSR/GAIN registers or the bank's envelope index; VxVOL with velocity ignore or scale_volume; MVOL and envelope counter phase",
                    "fm": "saved FM bank record or explicit registers on the OPN model with render_fm_patch's chip, clock, pan, LFO and sensitivity context; equal temperament from a4_hz with an optional fixed block; carrier TL velocity policy fixed or scale_carrier_tl"
                },
                "polyphony": "each note takes the lowest-numbered chip voice holding no note; at most 8 S-DSP voices, 6 OPN channels or 3 on YM2203 (max_voices may lower it); exceeding the limit refuses the render with chip_voices_exceeded",
                "rates": "chips run at their native rate (32000 Hz, or clock / prescaler / slots) and are converted to the render rate by the Kaiser-windowed sinc resampler; a 32000 Hz render of an S-DSP voice is not resampled",
                "controls": "note on/off, pitch and velocity are applied; program changes, pitch bend and other controllers are counted under not_applied",
                "channel_controls": "per instrument: ignore (default; CC7/CC10/CC11 under not_applied), level (CC7 x CC11 gain, (value/127)^2 each as the reference synthesizer, from CC7 100 and CC11 127) or level_and_pan (also CC10 equal-power pan, -3 dB per side at 64); steps at the controller's native sample before rate conversion and before the voice's effects and fader",
                "identity": "banks and plans must exist when the edit is made and are reloaded with identity checks at every render",
                "midi": "not written to MIDI; program numbers stay as authored and the export report lists the assignments",
                "source_backed": "allowed as render and bundle state; source MIDI bytes never change for it",
                "authority": "a model of the chip from public documentation, not the game's sound driver or hardware audio"
            },
            "audio_clips": {
                "operations": ["add_clip", "update_clip", "remove_clip"],
                "data": "clips in the revision's mixer (mix.clips): id, voice, source (SHA-256 of a WAV retained by load_instrument_source), start and duration in the timebase unit, optional source_start/source_end frames (trim), channels, gain_db, fade_in_ms and fade_out_ms; voice data under scope voices, range (the clip span) and protections",
                "source": "integer PCM 8/16/24/32-bit or IEEE float 32/64-bit WAV, plain or extensible, 1..32 channels, up to 64 MiB; identity checked when the edit is made and at every render",
                "channels": {"stereo": "two-channel source, left and right kept", "channel": "one 0-based source channel on both sides at unity (mono to stereo)", "downmix": "per-frame mean of every channel on both sides"},
                "rate": "resampled to the render rate by the Kaiser-windowed sinc resampler when the rates differ, unrounded; the render report states ratio and method",
                "placement": "the clip's first sample lands on the first render sample at or after its start time; audio stops at the span end, or earlier with silence when the source range runs out",
                "fades": "linear in amplitude over ceil(ms x rate / 1000) samples; the fade-out ends at the last audible sample; overlapping fades multiply",
                "mixing": "clips on one voice sum, then add to the voice's dry signal before its effects, fader, sends and output; mute and solo apply",
                "loops": "a clip may not cross loop_start (clip_crosses_loop_start) or end after the playback end; clips at or after loop_start replay in every loop pass",
                "excerpts": "the part of each clip inside the range, sample-identical to the full render's placement",
                "midi": "not written to MIDI; the export report lists clips and the bundle carries their records, with source files in objects/",
                "source_backed": "allowed as render and bundle state on existing voices; source MIDI bytes never change for it"
            },
            "bundles": "export_project carries the records revisions refer to (clip sources, instrument banks, FM banks, sample plans) in the bundle's references and their bytes in objects/<sha256> (referenced_data=omit leaves the bytes out); import_project verifies every record and object by hash and refuses, naming the reference, when one is neither carried nor retained",
            "loops": "one loop region [loop_start, end); no note may sound across loop_start, and tempo and controller values at the end must equal those at loop_start unless a change is placed at loop_start",
            "profiles": builtin_profiles(),
            "sound_sources": [{"name": REFERENCE_SOUND_SOURCE, "description": "generated looped square, pulse, triangle and saw waves by General MIDI family; shows structure, not chip timbre"}],
            "limits": {"events_page": MAX_PAGE, "max_tail_seconds": MAX_TAIL_SECONDS, "new_project_render_voices": 15, "imported_render_channels": 16, "sample_rate": [16000, 192000]},
    });
    let more = json!({
            "render_batch": {
                "spans": ["full (loop_passes)", "excerpt", "section by id", "every section"],
                "stems": {"voice": "everything one voice contributes", "bus": "each unmuted bus plus direct (voices routed to the master)", "group": "declared groups of voices, each voice in at most one"},
                "stem_master": {"through_master": "default; bus and master compressors and limiters apply the whole mix's gain to each stem, so stems sum to the mixdown within f32 rounding", "pre_master": "stems end before the master volume and chain and sum to the master input"},
                "approximate": "a stem sharing an SNES echo with other stems on a bus or the master; sum_check.approximated_by names it",
                "formats": {"pcm16": "default, as render_preview", "pcm24": "round(clamp(x) x 8388607)", "float32": "unchanged f32 samples, not clamped"},
                "max_renders_per_call": super::MAX_BATCH_RENDERS,
                "records": "each output is a render record (region, part, pre_master, format, sha256, played_by) usable by present_review, measure_audio and export_project render_ids, and regenerated by import_project",
                "regenerate_renders": "restores the audio of records imported with missing_renders without_audio, rendered again from each record and checked against its sha256; no record changes"
            },
            "markers_and_regions": "sections are named regions [start, end) (set_sections) rendered by id with render_batch; markers are named positions with a MIDI meta-event kind (set_markers); both need scope.timeline; export_project writes markers as meta events and, with sections_as_markers, each section id as a marker at its start",
            "sample_conversion": {
                "tools": ["prepare_sample", "inspect_sample", "export_sample"],
                "input": "saved original byte slice (raw or RIFF/WAVE) or native BRR bank slot; loading is independent",
                "wav": {"formats": "integer PCM 8/16/24/32-bit and IEEE float 32/64-bit, plain or WAVE_FORMAT_EXTENSIBLE, 1..32 channels, 8 MiB", "channels": "wav.channel or wav.downmix=mean (channel_downmix)", "reduction": "rounded half away from zero to PCM16; differences are bit_depth_reduction", "rate": "sample_rate must equal the header unless wav.reinterpret_rate (rate_reinterpreted)", "smpl": "loop points and root reported; used only with wav.smpl_loop / wav.smpl_root", "unknown_chunks": "listed, retained byte-exact, not interpreted"},
                "tuning": "optional root note and cents recorded; BRR targets report the 14-bit S-DSP pitch register round(rate * 4096 / 32000) and in-tune values",
                "resampling": "optional target_sample_rate; deterministic Kaiser-windowed sinc (beta 9, 32 zero crossings, cutoff 0.9 of lower Nyquist), reported as resampled; preserved loops must scale to whole and BRR-aligned samples",
                "codec": "built in (S-DSP integer BRR decoding, greedy BRR encoding; documented SPU ADPCM decoding, per-block filter/shift search); optional trusted external script with the same JSON protocol, snapshotted",
                "loop_policies": {"finite": "bake 0..16 additional traversals into a one-shot", "preserve": "keep the loop; BRR targets need 16-sample alignment and restart with filter 0; first pass plus one traversal compared, with difference locations; previews hold that stream with a smpl loop"},
                "formats": ["brr", "u8", "s8", "s16le", "s16be", "psx_adpcm", "vag"],
                "source_formats": ["brr", "u8", "s8", "s16le", "s16be", "psx_adpcm", "vag", "wav"],
                "variants": {
                    "snes_brr": {"formats": ["brr"], "block": "9 bytes, 16 samples; filters 0-3; end and loop flags on the last block; loop address from the bank or loop_offset", "decoder": "S-DSP integer arithmetic"},
                    "psx_spu_adpcm": {
                        "formats": ["psx_adpcm", "vag"],
                        "block": "16 bytes, 28 samples, low nibble first; shift 0-12 and filters 0-4 in the first byte; flags bit 0 loop end, bit 1 repeat, bit 2 loop start",
                        "decoder": "documented equation: (t << (12 - shift)) + ((old * pos + older * neg + 32) >> 6), pos 0/60/115/98/122, neg 0/0/-52/-55/-60, clamped to 16 bits; initial history zero",
                        "refused": "filters 5-15; shifts 13-15 unless psx_adpcm.reserved_shift_as_9; no loop-end block; blocks after it; repeat without a loop-start flag unless loop_offset declares the address; a loop address outside the slice",
                        "endings": {"end_mute": "loop end without repeat", "silent_loop_block": "documented one-shot terminator (filter 0, zero data, flags 7 on itself)", "loop": "loop end and repeat to the last loop-start block"},
                        "encoder": crate::psx_adpcm::SEARCH_RULE,
                        "targets": "one-shot: silent_loop_block appended (default) or end_mute on the last data block; preserved loop: 28-sample-aligned start and length, loop start flag and filter 0 on the loop block, loop end and repeat on the last block",
                        "vag": "VAGp only (VAGi and pGAV refused); big-endian version, data size (block bytes after the 48-byte header) and sample rate; reserved, name and trailing bytes reported and retained; a vag target from a vag source keeps every header byte except data size, sample rate and an explicit psx_adpcm.name",
                        "tuning": "SPU pitch register round(rate * 4096 / 44100), 1..0x4000",
                        "changes": ["reserved_shift_interpreted", "flag_bits_not_carried", "rate_reinterpreted"],
                        "authority": "public psx-spx documentation; a black-box FFmpeg 9 adpcm_psx decode agrees exactly on filter-0 blocks but rounds predictions differently; no SPU hardware or emulator comparison"
                    }
                },
                "limits": "mono samples, explicit rate, 131072 decoded samples, at most 16 additional loop traversals; no native insertion or playback",
                "preservation": "exact original retained; codec reports sample differences, padding, finite loop baking, resampling and WAV reductions; export requires exact change selection"
            },
            "observations": {
                "tools": ["attach_observation", "inspect_observation", "export_observation"],
                "producer_formats": ["portable", "emucap_audio"],
                "receipt": "producer, content identity keys, capability revision, clock cursors, completion and hashed members; unknown fields retained verbatim",
                "limitations": ["incomplete_capture", "capability_unrecorded", "capability_revision_changed", "stale_producer_build", "content_unverified", "clock_unrecorded", "undescribed_members"],
                "subjects": ["midi_source", "sound_source", "instrument_bank", "fm_bank", "sample_plan", "target_plan", "revision"],
                "limits": "256 regular files and 256 MiB per folder; no links; producer event streams are retained, not decoded",
                "authority": "evidence attached by the agent; loading never depends on an observation, and no game, driver or sound-equivalence claim follows"
            },
            "fm_banks": {
                "family": "opn",
                "tools": ["load_fm_bank", "inspect_fm_bank", "copy_fm_patch", "export_fm_bank", "render_fm_patch"],
                "scope": crate::fm::SCOPE,
                "layouts": "explicit absolute record offsets; operator rows or register columns; declared slot order; no game identities or address lists",
                "preservation": "complete original container; exact 29 interpreted register parameters; every unselected bit, reserved bit, record and opaque metadata retained",
                "limits": "no automatic bank discovery, reduced-register formats, native binding rewrite, sound-equivalence claim or hardware audio",
                "audition": {
                    "tool": "render_fm_patch",
                    "chips": {"ym2203": {"channels": 3, "native_rate": "clock / (prescaler * 12)"}, "ym2608": {"channels": 6, "native_rate": "clock / (prescaler * 24)"}, "ym2612": {"channels": 6, "native_rate": "clock / 144"}},
                    "common_clocks_hz": {"ym2612_ntsc_mega_drive": 7670454, "ym2612_pal_mega_drive": 7600489, "ym2608": 7987200, "ym2203": 3993600},
                    "context": "chip, clock and pan are declared; prescaler 6, disabled LFO, AMS/PMS 0, all operators keyed and SSG-EG refusal are reported defaults",
                    "model": crate::opn::model_scope(false)
                }
            },
            "psg": {
                "family": "sn76489",
                "tools": ["load_instrument_source", "inspect_vgm", "render_psg", "transform_vgm"],
                "variants": {"sega": crate::psg::PsgVariant::Sega.config().to_json(), "sn76489": crate::psg::PsgVariant::Sn76489.config().to_json()},
                "common_clocks_hz": {"ntsc": crate::psg::NTSC_CLOCK_HZ, "pal": crate::psg::PAL_CLOCK_HZ},
                "native_rate": "clock / 16, written as the rounded WAV header rate; optional Kaiser-windowed sinc resampling to 8000..192000 Hz",
                "vgm": "versions 1.00-1.71 retained exactly through load_instrument_source; header, layout, commands, kinds, loop and GD3 through inspect_vgm; render_psg applies 0x50 and 0x4F writes with waits 0x61-0x63, 0x7n and 0x8n, loops by the header loop offset and reports every other command as skipped; VGZ must be decompressed first",
                "inputs": "a retained VGM (header clock, feedback, width and flags) or explicit writes/notes with a declared variant and clock",
                "limits": format!("at most {} s per render and {} explicit writes; dual-chip second PSG, T6W28, XNOR noise and the /2 divider refuse or are reported as not rendered", crate::psg::MAX_RENDER_SECONDS, crate::psg::MAX_EVENTS),
                "model": crate::psg::model_scope()
            },
            "ay8910": {
                "family": "ay8910",
                "tools": ["load_instrument_source", "inspect_vgm", "render_psg", "transform_vgm"],
                "variants": {"ay8910": crate::ay::AyConfig::new(crate::ay::AyVariant::Ay8910).to_json(), "ym2149": crate::ay::AyConfig::new(crate::ay::AyVariant::Ym2149).to_json()},
                "common_clocks_hz": {"msx": crate::ay::MSX_CLOCK_HZ, "zx_spectrum_128": 1_773_400, "atari_st": 2_000_000},
                "clock_range_hz": [crate::ay::MIN_CLOCK_HZ, crate::ay::MAX_CLOCK_HZ],
                "native_rate": "clock / 8 (clock / 16 when the VGM flag halves a YM2149 clock)",
                "vgm": "header 0x74 clock with dual-chip bit, 0x78 chip type (AY8910/8912/8913, YM2149/YM3439/YMZ284/YMZ294; AY8930 refuses), 0x79 flags (pin-26 halving, ABC pan); 0xA0 writes (bit 7 selects the second chip) and the 0x31 stereo mask",
                "inputs": "a retained VGM, or explicit register writes (register 0..15) or notes (pitch and/or noise.period with volume) with variant ay8910 or ym2149 and clock_hz",
                "model": crate::ay::model_scope()
            },
            "gb_dmg": {
                "family": "gb_dmg",
                "tools": ["load_instrument_source", "inspect_vgm", "render_psg", "transform_vgm"],
                "variants": {"dmg": crate::gb_apu::GbConfig::dmg().to_json()},
                "common_clocks_hz": {"game_boy": crate::gb_apu::DMG_CLOCK_HZ},
                "clock_range_hz": [crate::gb_apu::MIN_CLOCK_HZ, crate::gb_apu::MAX_CLOCK_HZ],
                "native_rate": "clock / 32 (each sample the mean of 16 clock / 2 ticks)",
                "vgm": "header 0x80 clock with dual-chip bit; 0xB3 writes to offsets from 0xFF10 including wave RAM (bit 7 selects the second chip)",
                "inputs": "a retained VGM, or explicit register writes (offsets 0x00..0x2F, at_tick for exact clock / 2 timing) or notes (pulse, wave and noise channels) with variant dmg and clock_hz",
                "model": crate::gb_apu::model_scope()
            },
            "nes_apu": {
                "family": "nes_apu",
                "tools": ["load_instrument_source", "inspect_vgm", "render_psg", "transform_vgm"],
                "variants": {"nes_apu": crate::nes_apu::NesConfig::new(crate::nes_apu::NesRegion::Ntsc).to_json(), "nes_apu_pal": crate::nes_apu::NesConfig::new(crate::nes_apu::NesRegion::Pal).to_json()},
                "common_clocks_hz": {"ntsc_2a03": crate::nes_apu::NTSC_CLOCK_HZ, "pal_2a07": crate::nes_apu::PAL_CLOCK_HZ},
                "clock_range_hz": [crate::nes_apu::MIN_CLOCK_HZ, crate::nes_apu::MAX_CLOCK_HZ],
                "native_rate": "clock / 16 (each sample the mean of 16 CPU cycles)",
                "vgm": "header 0x84 clock with dual-chip bit 30 and FDS bit 31 (PAL tables within 1% of 1662607 Hz); 0xB4 writes to $4000..$401F (bit 7 selects the second chip; FDS registers skipped); 0xC2 data blocks fill DMC memory",
                "inputs": "a retained VGM, or explicit register writes (offsets 0x00..0x1F, at_tick in CPU cycles, optional dmc_memory) or notes (pulse, triangle, noise and DPCM channels) with variant nes_apu or nes_apu_pal and clock_hz",
                "limits": "expansion audio (FDS, VRC6, VRC7, MMC5, Namco 163, Sunsoft 5B) not modelled; output before the console's filters",
                "model": crate::nes_apu::model_scope()
            },
            "huc6280": {
                "family": "huc6280",
                "tools": ["load_instrument_source", "inspect_vgm", "render_psg"],
                "variants": {"huc6280": crate::huc6280::PceConfig::new(crate::huc6280::CLOCK_HZ).to_json()},
                "common_clocks_hz": {"pc_engine": crate::huc6280::CLOCK_HZ},
                "clock_range_hz": [crate::huc6280::MIN_CLOCK_HZ, crate::huc6280::MAX_CLOCK_HZ],
                "native_rate": "clock / 32 (each sample the mean of 32 clock cycles)",
                "vgm": "header 0xA4 clock with dual-chip bit 30; 0xB9 writes to R0..R9 ($0800..$0809; bit 7 selects the second chip); DAC streams with HuC6280 PCM data (type 0x05) are not rendered",
                "inputs": "a retained VGM, or explicit register writes (R0..R9, at_tick in clock cycles) or notes (wavetable tones on channels 0..5, noise on 4 and 5) with variant huc6280 and clock_hz",
                "limits": "output before the analog stage, DC kept; the noise sequence is a declared stand-in; PC Engine CD ADPCM and CD audio not modelled",
                "model": crate::huc6280::model_scope()
            },
            "wonderswan": {
                "family": "wonderswan",
                "tools": ["load_instrument_source", "inspect_vgm", "render_psg"],
                "variants": {"wonderswan": crate::wonderswan::WsConfig::new(crate::wonderswan::CLOCK_HZ).to_json()},
                "common_clocks_hz": {"wonderswan": crate::wonderswan::CLOCK_HZ},
                "clock_range_hz": [crate::wonderswan::MIN_CLOCK_HZ, crate::wonderswan::MAX_CLOCK_HZ],
                "native_rate": "clock / 128 (24000 Hz at 3072000 Hz; the mix sampled once per output sample)",
                "vgm": "header 0xC0 clock with dual-chip bit 30; 0xBC writes to ports $80..$9F (register = port - 0x80; bit 7 selects the second chip); 0xC6 writes internal RAM, where the wavetables live",
                "inputs": "a retained VGM, or explicit port writes (register 0x00..0x1F) and RAM writes (address), at_tick in CPU cycles, or notes (wavetable tones on channels 0..3, noise on 3) with variant wonderswan and clock_hz",
                "limits": "the headphone path only (the internal speaker and its volume are not rendered); Hyper Voice and the sound test port are not modelled",
                "model": crate::wonderswan::model_scope()
            },
            "vgm_mixing": "render_psg renders every modelled chip a VGM declares; several chips need output_sample_rate and are each resampled, scaled by 1/N and summed (reported as mix); families selects some of them",
            "vgm_transformation": {
                "tool": "transform_vgm",
                "copy": "one declared family (sn76489, ay8910, gb_dmg, nes_apu): transpose_semitones by period rewrite, mute_channels, clock_hz with keep_pitch, variant (sega/sn76489, ay8910/ym2149, nes_apu/nes_apu_pal); other commands copied byte for byte",
                "map": "map_sn76489_to_ay: SN76489 tones and noise to AY-3-8910/YM2149 channels; no other cross-family mapping",
                "losses": "each unrepresentable change is a named loss code; the call refuses unless accept_losses lists it",
                "output": "a new retained source with derived_from provenance, transformed.vgm and transform.json; waits unchanged; parsed and replayed against the plan"
            },
            "instrument_banks": {
                "tools": ["load_instrument_source", "inspect_instrument_source", "export_instrument_source", "load_instrument_bank", "inspect_instrument_bank", "copy_instrument", "export_instrument_bank"],
                "discovery": "inspect_instrument_bank view=banks lists saved immutable IDs",
                "scope": "BRR sample reuse with exact runtime identity or reviewed Compile standard semantic translation; explicit layouts, source discovery and native track bindings",
                "preservation": "no re-encoding; exact BRR bytes and active loop offset, exact pitch/envelope program contents with index relocation; unrelated sample and referenced table data unchanged",
                "placement": "reuse byte-identical samples, append in declared free memory, or explicitly replace a large exclusive slot; aliases and reserved slots protected",
                "delivery": "export_instrument_bank delivers native modules independently of MIDI; the existing Compile MIDI encoder backend accepts only its declared layout",
                "limits": "no arbitrary driver interpretation, mixed audio separation or automatic loudness normalization; native edits change initial bindings, not later score commands; audition through render_instrument"
            },
            "instrument_audition": {
                "tools": ["render_instrument"],
                "renderer": "built-in S-DSP voice model from public hardware documentation; one voice, 32000 Hz nominal stereo 16-bit WAV",
                "inputs": "saved bank slot (exact BRR and loop) or sample plan with a BRR target; pitch register or note against an explicit root register; ADSR/GAIN registers or the bank's envelope index; key-on and release lengths in DSP samples; VxVOL, MVOL and envelope counter phase",
                "modelled": "BRR filters with carried history, 12-sample decode ring, Gaussian interpolation, five-sample key-on delay, end/loop flags, ADSR/GAIN rates and counter offsets, KOFF release, voice clamp and master volume",
                "not_modelled": "echo/FIR, noise, pitch modulation, driver pitch arithmetic, vibrato and volume programs, the console's real sample clock (about 32040 Hz on observed hardware emulation)",
                "limits": format!("at most {} DSP samples per render", crate::snes_dsp::MAX_SAMPLES)
            },
            "target_conversion": {
                "score_tools": ["prepare_target", "inspect_target", "export_target"],
                "backend": "Compile score module API with external encoder, template and bank; Python 3 standard library",
                "inputs": "loaded synchronous PPQ MIDI 0/1 or project revision; no editable projection required for sources",
                "mapping": "one to eight tracks, channels/chord rank, sample/envelope, drums, pan/volume, timing, loop, delayed copies; coincident onsets refuse unless same_tick=last is explicit",
                "verification": "every source event accounted, encoded tracks independently decoded, headers/prefix/ARAM bounds checked, immutable input/tool/output hashes",
                "selection": "export requires the exact plan and its explicit change kinds; original bytes remain separate and exact",
                "limits": "this backend reports rather than transfers MIDI controls/velocity/metadata; no original-device audio or ROM-playback claim",
                "legacy_frame_adapter": "convert_for_target still refuses source MIDI and known mapping losses; use the score workflow for supported explicit source mappings"
            },
            "measurement": {
                "tools": ["measure_audio", "compare_audio"],
                "inputs": "stored renders by project and render ID, or workspace WAV files: integer PCM 8/16/24/32-bit, float 32/64-bit, plain or extensible, 1-8 channels, 8-384 kHz",
                "metrics": ["sample peak dBFS", "true peak dBTP (4x Kaiser-windowed sinc oversampling)", "RMS (mean square, full-scale sine -3.01 dB)", "BS.1770-4 integrated loudness with -70 LUFS absolute and -10 LU relative gates", "momentary (400 ms) and short-term (3 s) maxima", "EBU Tech 3342 loudness range", "DC offset", "clipped samples", "leading and trailing silence", "octave or third-octave band levels, with DC-removed shares when DC carries at least 0.1% of the power", "optional YIN fundamental frequency per window with nearest note, cents, periodicity, note segments and summary (pitch)"],
                "comparison": "same rate and channel count; offset search up to ±2 s by normalized cross-correlation; sample differences, gain-matched residual, band level and loudness differences; optional pitch differences in cents at the found alignment",
                "limits": {"wav_bytes": crate::measure::MAX_WAV_BYTES, "sample_values_per_input": crate::measure::MAX_SAMPLE_VALUES, "max_offset_seconds": crate::measure::MAX_OFFSET_SECONDS, "fft_size": crate::measure::spectrum::MAX_FFT},
                "authority": "numbers describe the signal; they are not a listening judgment or a target-chip loudness claim"
            },
            "feedback": "Marks are observations tied to a render and PCM sample; they never change music, permissions or approval.",
    });
    if let (Some(all), Value::Object(more)) = (capabilities.as_object_mut(), more) {
        all.extend(more);
    }
    capabilities
}
