//! Applying edit requests to revisions.

use super::*;

impl Service {
    pub(super) fn edit_project(&mut self, input: EditProjectInput) -> Handled {
        let (mut info, settings) = self.project(&input.project)?;
        let request = input.request;
        let Some(base) = self
            .store_ref()?
            .reader()
            .revision(&input.project, &request.base_revision)?
        else {
            return done(ToolResponse::refused(
                Code::UnknownReference,
                "/request/base_revision",
                format!("revision `{}` does not exist", request.base_revision),
            ));
        };
        if request.base_revision != info.head_revision && !input.branch {
            return done(ToolResponse::refused(
                Code::StaleRevision,
                "/request/base_revision",
                format!(
                    "the head is `{}`, not `{}`; edit the head or set branch to continue from the older revision",
                    info.head_revision, request.base_revision
                ),
            ));
        }
        let known_feedback: Vec<Feedback> = self
            .store_ref()?
            .reader()
            .documents(&input.project, "feedback")?;
        for (i, feedback) in request.feedback.iter().enumerate() {
            if !known_feedback.iter().any(|f| &f.id == feedback) {
                return done(ToolResponse::refused(
                    Code::UnknownReference,
                    &format!("/request/feedback/{i}"),
                    format!("feedback `{feedback}` was not delivered"),
                ));
            }
        }
        let stacked = match info.midi_source {
            Some(_) => crate::composition::StackedNotes::SourcePaired,
            None => crate::composition::StackedNotes::Reject,
        };
        let result =
            match crate::edit::apply_edit_with(info.timebase, &base.composition, &request, stacked)
            {
                Ok(result) => result,
                Err(diagnostics) => {
                    return done(ToolResponse::blocked(
                        "the edit was not applied; nothing changed",
                        diagnostics,
                    ));
                }
            };
        if let Some(source) = &info.midi_source {
            if let Err(diagnostics) =
                crate::midi::check_source_bounds(source, &base.composition, &result, &request)
            {
                return done(ToolResponse::blocked(
                    "the edit was not applied; nothing changed",
                    diagnostics,
                ));
            }
            if let Err(diagnostics) =
                crate::midi::check_source_revision(source, info.timebase, &result)
            {
                return done(ToolResponse::blocked(
                    "the edit cannot be preserved in MIDI; nothing changed",
                    diagnostics,
                ));
            }
        }
        // Declared-end notes this revision's MIDI no longer leaves as in the source.
        let declared = match &info.midi_source {
            Some(source) if source.declared_ends.is_some() => {
                crate::midi::declared_end_changes(source, &result, false)
                    .map_err(|d| ToolResponse::blocked("invalid preserved MIDI", d))?
            }
            _ => vec![],
        };
        self.check_changed_instruments(&base.composition.mix, &result.mix)?;
        let clips = self.check_changed_clips(info.timebase, &base.composition, &result)?;
        let store = self.store_ref()?;
        let id = format!("r{}", store.reader().revision_count(&input.project)? + 1);
        let difference = compare_compositions(&base.composition, &result, None, None);
        let advisories = polyphony_advisories(&base.composition, &result);
        let revision = Revision {
            id: id.clone(),
            parent: Some(base.id.clone()),
            edit_request: Some(request),
            composition: result,
        };
        info.head_revision = id.clone();
        let project = input.project.clone();
        let mut summary = format!(
            "{id} from {}: {} added, {} removed, {} changed notes, {} control lanes, timeline {:?}",
            base.id,
            difference.added_notes.len(),
            difference.removed_notes.len(),
            difference.changed_notes.len(),
            difference.changed_controls.len(),
            difference.changed_timeline
        );
        if !difference.changed_mix.is_empty() {
            summary.push_str(&format!(", mix {:?}", difference.changed_mix));
        }
        let mut response = ToolResponse::ok(
            summary,
            json!({"revision": id, "parent": base.id, "difference": difference}),
        );
        if !advisories.is_empty() {
            response.result["advisories"] = json!(advisories);
        }
        if !clips.is_empty() {
            response.result["clips"] = json!(clips);
        }
        if !declared.is_empty() {
            response.result["declared_end_writes"] = json!({"notes": declared,
                "scope": "every declared-end note this revision changes relative to the source; its MIDI writes these releases"});
        }
        commit(response, move |writer| {
            writer.insert_revision(&project, &revision)?;
            writer.update_project(&info, &settings)
        })
    }
}

/// Most notes sounding at once in each voice, with the first tick reaching it.
fn voice_polyphony(composition: &Composition) -> std::collections::BTreeMap<&str, (usize, u64)> {
    let mut edges: std::collections::BTreeMap<&str, Vec<(u64, i8)>> = Default::default();
    for note in &composition.notes {
        let voice = edges.entry(note.voice.as_str()).or_default();
        voice.push((note.start, 1));
        voice.push((note.end().unwrap_or(u64::MAX), -1));
    }
    edges
        .into_iter()
        .map(|(voice, mut edges)| {
            // Ends sort before starts at the same tick, so touching notes don't overlap.
            edges.sort();
            let (mut now, mut most) = (0i64, (0usize, 0u64));
            for (at, change) in edges {
                now += change as i64;
                if now as usize > most.0 {
                    most = (now as usize, at);
                }
            }
            (voice, most)
        })
        .collect()
}

/// Non-blocking notes for the agent: voices whose simultaneous-note count grew,
/// such as a single line made to overlap by humanize or strum.
fn polyphony_advisories(base: &Composition, result: &Composition) -> Vec<Value> {
    let before = voice_polyphony(base);
    voice_polyphony(result)
        .into_iter()
        .filter_map(|(voice, (after, at))| {
            let was = before.get(voice).map_or(0, |b| b.0);
            (after > was && after > 1).then(|| {
                json!({"kind": "voice_polyphony_increased", "voice": voice, "before": was, "after": after, "first_at": at,
                    "note": "valid polyphony; if this voice should stay a single line, use legato_notes or smaller offsets and re-run check_project"})
            })
        })
        .collect()
}
