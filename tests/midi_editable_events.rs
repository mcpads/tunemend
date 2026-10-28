//! Source-backed MIDI editing of stacked, unpaired and shared-channel events.
use midly::num::{u4, u7, u15, u28};
use midly::{Format, Header, MetaMessage, MidiMessage, Smf, Timing, TrackEvent, TrackEventKind};
use serde_json::{Value, json};
use tunemend::diagnostic::DiagnosticCode as Code;
use tunemend::midi::{read_smf, read_smf_preserving};
use tunemend::service::{Service, Status};

type Events = Vec<(u32, TrackEventKind<'static>)>;

fn midi(channel: u8, message: MidiMessage) -> TrackEventKind<'static> {
    TrackEventKind::Midi {
        channel: u4::new(channel),
        message,
    }
}
fn on(channel: u8, key: u8, velocity: u8) -> TrackEventKind<'static> {
    midi(
        channel,
        MidiMessage::NoteOn {
            key: u7::new(key),
            vel: u7::new(velocity),
        },
    )
}
fn off(channel: u8, key: u8) -> TrackEventKind<'static> {
    midi(
        channel,
        MidiMessage::NoteOff {
            key: u7::new(key),
            vel: u7::new(64),
        },
    )
}
fn cc(channel: u8, controller: u8, value: u8) -> TrackEventKind<'static> {
    midi(
        channel,
        MidiMessage::Controller {
            controller: u7::new(controller),
            value: u7::new(value),
        },
    )
}
/// Absolute-tick events per track, each ending with end-of-track at `end`.
fn file(tracks: Vec<Events>, end: u32) -> Vec<u8> {
    let tracks = tracks
        .into_iter()
        .map(|events| {
            let mut previous = 0;
            let mut track: Vec<TrackEvent> = events
                .into_iter()
                .map(|(at, kind)| {
                    let delta = at - previous;
                    previous = at;
                    TrackEvent {
                        delta: u28::new(delta),
                        kind,
                    }
                })
                .collect();
            track.push(TrackEvent {
                delta: u28::new(end - previous),
                kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
            });
            track
        })
        .collect();
    let format = if matches!(&tracks, t if Vec::len(t) == 1) {
        Format::SingleTrack
    } else {
        Format::Parallel
    };
    let smf = Smf {
        header: Header::new(format, Timing::Metrical(u15::new(480))),
        tracks,
    };
    let mut bytes = Vec::new();
    smf.write_std(&mut bytes).unwrap();
    bytes
}
/// Per track: absolute tick and event, with note-on velocity zero as a release.
fn events(bytes: &[u8]) -> Vec<Vec<(u64, TrackEventKind<'_>)>> {
    Smf::parse(bytes)
        .unwrap()
        .tracks
        .into_iter()
        .map(|track| {
            let mut tick = 0;
            track
                .into_iter()
                .map(|event| {
                    tick += u64::from(event.delta.as_int());
                    (tick, event.kind)
                })
                .collect()
        })
        .collect()
}

struct Work {
    root: std::path::PathBuf,
    service: Service,
}

impl Work {
    fn new(name: &str, bytes: &[u8]) -> Self {
        Self::importing(name, bytes, json!({})).0
    }
    /// Imports with extra `import_midi` arguments and returns its response.
    fn importing(
        name: &str,
        bytes: &[u8],
        extra: Value,
    ) -> (Self, tunemend::service::ToolResponse) {
        let root = std::env::temp_dir().join(format!("tunemend-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("song.mid"), bytes).unwrap();
        let mut service = Service::new(&root);
        let mut request = json!({"job_id": "import", "project": "song", "path": "song.mid"});
        for (key, value) in extra.as_object().unwrap() {
            request[key] = value.clone();
        }
        let imported = service.call("import_midi", request);
        assert_eq!(imported.status, Status::Ok, "{imported:?}");
        (Self { root, service }, imported)
    }
    fn edit(&mut self, job: &str, base: &str, request: Value) -> tunemend::service::ToolResponse {
        let mut request = request;
        request["job_id"] = json!(job);
        request["base_revision"] = json!(base);
        self.service.call(
            "edit_project",
            json!({"project": "song", "request": request, "branch": true}),
        )
    }
    fn head(&mut self) -> String {
        let summary = self.service.call(
            "inspect_project",
            json!({"project": "song", "view": "summary"}),
        );
        summary.result["head_revision"].as_str().unwrap().to_owned()
    }
    fn export(&mut self, job: &str) -> Vec<u8> {
        let exported = self.service.call(
            "export_project",
            json!({"job_id": format!("export-{job}"), "project": "song", "destination": format!("out/{job}"), "render_audio": "none"}),
        );
        assert_eq!(exported.status, Status::Ok, "{exported:?}");
        std::fs::read(self.root.join(format!("out/{job}/song.mid"))).unwrap()
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn codes(response: &tunemend::service::ToolResponse) -> Vec<Code> {
    response.diagnostics.iter().map(|d| d.code).collect()
}

#[test]
fn stacked_onsets_pair_first_in_first_out_and_keep_their_own_events() {
    // Key 60 starts twice before either release; key 62 doubles at one tick.
    let bytes = file(
        vec![vec![
            (0, on(0, 60, 100)),
            (100, on(0, 60, 90)),
            (200, off(0, 60)),
            (300, off(0, 60)),
            (400, on(0, 62, 80)),
            (400, on(0, 62, 70)),
            (500, off(0, 62)),
            (600, off(0, 62)),
        ]],
        960,
    );
    let strict = read_smf(&bytes).unwrap_err();
    assert_eq!(
        strict.iter().map(|d| d.code).collect::<Vec<_>>(),
        vec![Code::OverlappingNotes, Code::OverlappingNotes],
        "strict reading names each stacked onset without cascading release errors: {strict:#?}"
    );
    let imported = read_smf_preserving(&bytes).unwrap();
    let notes: Vec<_> = imported
        .composition
        .notes
        .iter()
        .map(|n| (n.id.as_str(), n.start, n.duration, n.velocity))
        .collect();
    assert_eq!(
        notes,
        [
            ("n1", 0, 200, 100),
            ("n2", 100, 200, 90),
            ("n3", 400, 100, 80),
            ("n4", 400, 200, 70)
        ]
    );
    assert_eq!(imported.note_bindings[3].onset, (0, 5));
    assert_eq!(imported.note_bindings[3].release, Some((0, 7)));
    assert!(
        imported.retained.iter().any(
            |r| r.location == "/tracks/0/events/1" && r.message.contains("first in, first out")
        )
    );

    let mut work = Work::new("stacked", &bytes);
    assert_eq!(work.export("unchanged"), bytes);
    // The second of two identical same-tick onsets changes, not the first.
    let edited = work.edit(
        "velocity",
        "r1",
        json!({"operations": [{"op": "update_notes", "notes": [{"id": "n4", "velocity": 33}]}]}),
    );
    assert_eq!(edited.status, Status::Ok, "{edited:?}");
    let output = work.export("velocity");
    let (before, after) = (events(&bytes), events(&output));
    let changed: Vec<usize> = (0..before[0].len())
        .filter(|&i| before[0][i] != after[0][i])
        .collect();
    assert_eq!(changed, [5]);
    assert_eq!(after[0][5], (400, on(0, 62, 33)));
    // A stacked note's release moves with its own identity.
    let shorter = work.edit(
        "shorter",
        "r1",
        json!({"operations": [{"op": "update_notes", "notes": [{"id": "n2", "duration": 150}]}]}),
    );
    assert_eq!(shorter.status, Status::Ok, "{shorter:?}");
    let output = work.export("shorter");
    assert_eq!(events(&output)[0][3], (250, off(0, 60)));
}

#[test]
fn ambiguous_or_new_stacks_refuse_without_a_revision() {
    let bytes = file(
        vec![vec![
            (0, on(0, 60, 100)),
            (100, on(0, 60, 90)),
            (200, off(0, 60)),
            (300, off(0, 60)),
            (400, on(0, 64, 80)),
            (500, off(0, 64)),
        ]],
        960,
    );
    let mut work = Work::new("ambiguous", &bytes);
    // Ending the earlier note after the later one would pair the other way.
    let swapped = work.edit(
        "swap",
        "r1",
        json!({"operations": [{"op": "update_notes", "notes": [{"id": "n1", "duration": 350}]}]}),
    );
    assert_eq!(swapped.status, Status::Blocked, "{swapped:?}");
    assert_eq!(work.head(), "r1");
    // Retrying the same job returns the recorded refusal; nothing was applied.
    let retried = work.edit(
        "swap",
        "r1",
        json!({"operations": [{"op": "update_notes", "notes": [{"id": "n1", "duration": 350}]}]}),
    );
    assert_eq!(retried.status, Status::Blocked);
    assert!(retried.replayed);
    // A stack the source did not contain is refused, together with an
    // otherwise valid change in the same request.
    let created = work.edit(
        "create",
        "r1",
        json!({"operations": [
            {"op": "update_notes", "notes": [{"id": "n1", "velocity": 10}]},
            {"op": "update_notes", "notes": [{"id": "n3", "pitch": 60, "start": 250}]}
        ]}),
    );
    assert_eq!(created.status, Status::Blocked, "{created:?}");
    assert!(
        codes(&created).contains(&Code::OverlappingNotes),
        "{created:?}"
    );
    assert_eq!(work.head(), "r1");
    assert_eq!(work.export("still"), bytes);
}

#[test]
fn unpaired_and_zero_length_events_are_retained_without_invented_durations() {
    let bytes = file(
        vec![vec![
            (0, off(0, 50)),
            (0, on(0, 60, 100)),
            (240, off(0, 60)),
            (300, on(0, 36, 90)),
            (300, off(0, 36)),
            (480, on(0, 60, 80)),
            (720, off(0, 60)),
            (800, on(0, 72, 70)),
            (960, on(0, 74, 70)),
        ]],
        960,
    );
    let imported = read_smf_preserving(&bytes).unwrap();
    assert_eq!(imported.composition.notes.len(), 2);
    let retained: Vec<_> = imported
        .retained_note_events
        .iter()
        .map(|e| (e.event, e.tick))
        .collect();
    assert_eq!(
        retained,
        [
            ((0, 0), 0),
            ((0, 3), 300),
            ((0, 4), 300),
            ((0, 7), 800),
            ((0, 8), 960)
        ]
    );
    for (location, needle) in [
        ("/tracks/0/events/0", "without sounding"),
        ("/tracks/0/events/3", "zero-length"),
        ("/tracks/0/events/7", "never released"),
    ] {
        assert!(
            imported
                .retained
                .iter()
                .any(|r| r.location == location && r.message.contains(needle)),
            "{location}: {:#?}",
            imported.retained
        );
    }
    let mut work = Work::new("unpaired", &bytes);
    assert_eq!(work.export("unchanged"), bytes);
    let edited = work.edit(
        "edit",
        "r1",
        json!({"operations": [{"op": "update_notes", "notes": [{"id": "n2", "pitch": 62, "velocity": 50}]}]}),
    );
    assert_eq!(edited.status, Status::Ok, "{edited:?}");
    let output = work.export("edited");
    let after = events(&output);
    let before = events(&bytes);
    for index in [0, 3, 4, 7, 8] {
        assert_eq!(after[0][index], before[0][index]);
    }
    assert_eq!(after[0][5], (480, on(0, 62, 50)));
    // Moving a note so a retained onset would pair with its release refuses.
    let captured = work.edit(
        "capture",
        "r1",
        json!({"operations": [{"op": "update_notes", "notes": [{"id": "n2", "pitch": 72, "start": 850, "duration": 50}]}]}),
    );
    assert_eq!(captured.status, Status::Blocked, "{captured:?}");
    assert_eq!(work.head(), "r2");
}

#[test]
fn shared_channel_controls_are_checked_against_every_voice_on_the_channel() {
    // Tracks 1 and 2 both use channel 1; track 1 holds the volume control.
    let bytes = file(
        vec![
            vec![],
            vec![
                (0, cc(0, 7, 100)),
                (0, on(0, 60, 100)),
                (480, off(0, 60)),
                (480, cc(0, 7, 90)),
            ],
            vec![
                (240, on(0, 60, 80)),
                (720, off(0, 60)),
                (720, on(0, 64, 80)),
                (900, off(0, 64)),
            ],
        ],
        960,
    );
    assert!(
        read_smf(&bytes)
            .unwrap_err()
            .iter()
            .any(|d| d.code == Code::SharedChannelControl)
    );
    let imported = read_smf_preserving(&bytes).unwrap();
    assert!(
        imported
            .retained
            .iter()
            .any(|r| r.location == "/tracks/1/events/0"
                && r.message.contains("every voice on the channel"))
    );
    assert!(
        imported
            .retained
            .iter()
            .any(|r| r.location == "/tracks/2/events/0"
                && r.message.contains("also sounds from track 1"))
    );
    let mut work = Work::new("shared", &bytes);
    assert_eq!(work.export("unchanged"), bytes);
    let lane = json!({"op": "set_control_points", "voice": "track1-ch1", "kind": "volume",
        "points": [{"at": 0, "value": 80}, {"at": 480, "value": 90}]});
    let narrow = work.edit(
        "narrow",
        "r1",
        json!({"scope": {"voices": ["track1-ch1"]}, "operations": [lane]}),
    );
    assert_eq!(narrow.status, Status::Blocked, "{narrow:?}");
    assert!(codes(&narrow).contains(&Code::OutOfScope));
    let protected = work.edit(
        "protected",
        "r1",
        json!({"protections": [{"voice": "track2-ch1", "range": {"start": 240, "end": 480}}], "operations": [lane]}),
    );
    assert_eq!(protected.status, Status::Blocked, "{protected:?}");
    assert!(codes(&protected).contains(&Code::ProtectedChange));
    // A protection outside the changed value's span does not block it.
    let allowed = work.edit(
        "allowed",
        "r1",
        json!({"scope": {"voices": ["track1-ch1", "track2-ch1"]},
            "protections": [{"voice": "track2-ch1", "range": {"start": 720, "end": 960}}], "operations": [lane]}),
    );
    assert_eq!(allowed.status, Status::Ok, "{allowed:?}");
    let output = work.export("allowed");
    let (before, after) = (events(&bytes), events(&output));
    assert_eq!(before[0], after[0]);
    assert_eq!(before[2], after[2]);
    assert_eq!(after[1][0], (0, cc(0, 7, 80)));
    assert_eq!(after[1][1..], before[1][1..]);
    // A release on a shared key can end the other voice's note at a receiver.
    let release = work.edit(
        "release",
        "r1",
        json!({"protections": [{"voice": "track2-ch1"}],
            "operations": [{"op": "update_notes", "notes": [{"id": "n1", "duration": 300}]}]}),
    );
    assert_eq!(release.status, Status::Blocked, "{release:?}");
    assert!(codes(&release).contains(&Code::ProtectedChange));
    assert_eq!(work.head(), "r2");
}

#[test]
fn same_tick_controls_and_onsets_keep_their_source_order() {
    // The onset precedes the volume change at tick 0 in the source.
    let bytes = file(
        vec![vec![
            (0, on(0, 60, 100)),
            (0, cc(0, 7, 90)),
            (240, off(0, 60)),
            (480, on(0, 62, 100)),
            (720, off(0, 62)),
        ]],
        960,
    );
    let mut work = Work::new("order", &bytes);
    let edited = work.edit(
        "value",
        "r1",
        json!({"operations": [
            {"op": "set_control_points", "voice": "track0-ch1", "kind": "volume",
                "points": [{"at": 0, "value": 70}, {"at": 480, "value": 50}]},
            {"op": "update_notes", "notes": [{"id": "n2", "velocity": 60}]}
        ]}),
    );
    assert_eq!(edited.status, Status::Ok, "{edited:?}");
    let output = work.export("value");
    assert_eq!(
        events(&output)[0][..6],
        [
            (0, on(0, 60, 100)),
            (0, cc(0, 7, 70)),
            (240, off(0, 60)),
            // A new point precedes an onset at its tick, as new music exports.
            (480, cc(0, 7, 50)),
            (480, on(0, 62, 60)),
            (720, off(0, 62)),
        ]
    );
}

#[test]
fn admitted_sources_survive_bundle_resumption_and_reimport() {
    let bytes = file(
        vec![
            vec![
                (0, on(9, 38, 100)),
                (0, on(9, 38, 90)),
                (60, off(9, 38)),
                (120, off(9, 38)),
            ],
            vec![
                (0, cc(9, 7, 100)),
                (240, on(9, 42, 80)),
                (900, on(9, 42, 70)),
            ],
            vec![(480, on(9, 36, 100)), (600, off(9, 36))],
        ],
        960,
    );
    let mut work = Work::new("resume", &bytes);
    let edited = work.edit(
        "edit",
        "r1",
        json!({"operations": [{"op": "update_notes", "notes": [{"id": "n2", "velocity": 20}, {"id": "n3", "pitch": 35}]}]}),
    );
    assert_eq!(edited.status, Status::Ok, "{edited:?}");
    let output = work.export("delivery");
    let reimported = read_smf_preserving(&output).unwrap();
    let composition: Value = work
        .service
        .call(
            "inspect_project",
            json!({"project": "song", "view": "notes"}),
        )
        .result;
    assert_eq!(
        reimported.composition.notes.len(),
        composition["notes"].as_array().map_or(3, Vec::len)
    );
    assert_eq!(
        reimported.retained_note_events.len(),
        read_smf_preserving(&bytes)
            .unwrap()
            .retained_note_events
            .len()
    );
    let mut resumed = Service::new(work.root.join("resumed"));
    let restored = resumed.call(
        "import_project",
        json!({"job_id": "resume", "path": work.root.join("out/delivery/song.project.json")}),
    );
    assert_eq!(restored.status, Status::Ok, "{restored:?}");
    let again = resumed.call(
        "export_project",
        json!({"job_id": "again", "project": "song", "destination": "out/again", "render_audio": "none"}),
    );
    assert_eq!(again.status, Status::Ok, "{again:?}");
    assert_eq!(
        std::fs::read(work.root.join("resumed/out/again/song.mid")).unwrap(),
        output
    );
}

#[test]
fn a_track_count_mismatch_refuses_at_the_header_with_its_counts() {
    let mut bytes = file(
        vec![
            vec![(0, on(0, 60, 100)), (240, off(0, 60))],
            vec![(0, on(1, 62, 100)), (240, off(1, 62))],
        ],
        480,
    );
    bytes[10..12].copy_from_slice(&1u16.to_be_bytes());
    let refusal = read_smf_preserving(&bytes).unwrap_err();
    assert_eq!(refusal.len(), 1);
    assert_eq!(refusal[0].location, "/header");
    assert!(
        refusal[0]
            .message
            .contains("declares 1 tracks but the file holds 2 track chunks"),
        "{refusal:?}"
    );
}

#[test]
fn declared_ends_make_unreleased_and_zero_length_notes_editable_only_when_selected() {
    let bytes = file(
        vec![vec![
            (0, on(0, 60, 100)),
            (240, off(0, 60)),
            (300, on(0, 72, 90)),
            (400, on(0, 36, 80)),
            (400, off(0, 36)),
            (600, on(0, 72, 70)),
        ]],
        960,
    );
    // Without the option nothing is completed.
    assert_eq!(
        read_smf_preserving(&bytes).unwrap().composition.notes.len(),
        1
    );
    let (mut work, imported) = Work::importing(
        "declared",
        &bytes,
        json!({"declared_ends": {"end": "next_onset"}}),
    );
    let declared = &imported.result["declared_ends"];
    assert_eq!(declared["rule"], json!({"end": "next_onset"}));
    // The editing summary describes this projection, not the default one.
    assert_eq!(
        imported.result["editing"]["retained"],
        imported.result["retained"]
    );
    let ends: Vec<_> = declared["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| {
            (
                n["note"].clone(),
                n["from"].clone(),
                n["declared_end"].clone(),
            )
        })
        .collect();
    assert_eq!(
        ends,
        [
            (json!("n2"), json!("never_released"), json!(600)),
            (json!("n3"), json!("zero_length"), json!(960)),
            (json!("n4"), json!("never_released"), json!(960)),
        ]
    );
    assert_eq!(work.export("unchanged"), bytes);
    // An edit elsewhere leaves the declared notes' source events as they are.
    let other = work.edit(
        "other",
        "r1",
        json!({"operations": [{"op": "update_notes", "notes": [{"id": "n1", "velocity": 50}]}]}),
    );
    assert_eq!(other.status, Status::Ok, "{other:?}");
    assert!(other.result.get("declared_end_writes").is_none());
    let output = work.export("other");
    assert_eq!(events(&output)[0][2..], events(&bytes)[0][2..]);
    // Changing a declared note writes, and reports, its release.
    let shorter = work.edit(
        "shorter",
        "r1",
        json!({"operations": [{"op": "update_notes", "notes": [{"id": "n2", "duration": 100}, {"id": "n3", "pitch": 38, "duration": 50}]}]}),
    );
    assert_eq!(shorter.status, Status::Ok, "{shorter:?}");
    let writes = &shorter.result["declared_end_writes"]["notes"];
    assert_eq!(writes[0]["note"], "n2");
    assert_eq!(writes[0]["release_written_at"], 400);
    assert!(
        writes[0]["change"]
            .as_str()
            .unwrap()
            .contains("wrote a release")
    );
    assert!(
        writes[1]["change"]
            .as_str()
            .unwrap()
            .contains("moved the zero-length release")
    );
    let output = work.export("shorter");
    let after = events(&output);
    assert!(after[0].contains(&(400, off(0, 72))));
    assert!(after[0].contains(&(400, on(0, 38, 80))));
    assert!(after[0].contains(&(450, off(0, 38))));
    assert!(after[0].contains(&(600, on(0, 72, 70))));
    // The rule travels with the project's source through bundle resumption.
    let bundle: Value = serde_json::from_slice(
        &std::fs::read(work.root.join("out/shorter/song.project.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        bundle["project"]["midi_source"]["declared_ends"],
        json!({"end": "next_onset"})
    );
    let mut resumed = Service::new(work.root.join("resumed"));
    let restored = resumed.call(
        "import_project",
        json!({"job_id": "resume", "path": work.root.join("out/shorter/song.project.json")}),
    );
    assert_eq!(restored.status, Status::Ok, "{restored:?}");
    let again = resumed.call(
        "export_project",
        json!({"job_id": "again", "project": "song", "destination": "out/again", "render_audio": "none"}),
    );
    assert_eq!(again.status, Status::Ok, "{again:?}");
    assert_eq!(
        std::fs::read(work.root.join("resumed/out/again/song.mid")).unwrap(),
        output
    );
    // A fixed rule shortens to the track end; a zero length is refused.
    let fixed = tunemend::midi::read_smf_declaring(
        &bytes,
        Some(serde_json::from_value(json!({"end": "fixed", "ticks": 500})).unwrap()),
    )
    .unwrap();
    let durations: Vec<_> = fixed
        .composition
        .notes
        .iter()
        .map(|n| (n.start, n.duration))
        .collect();
    assert_eq!(durations, [(0, 240), (300, 500), (400, 500), (600, 360)]);
    let refused = work.service.call(
        "import_midi",
        json!({"job_id": "zero", "project": "zero", "path": "song.mid", "declared_ends": {"end": "fixed", "ticks": 0}}),
    );
    assert_eq!(refused.status, Status::Blocked, "{refused:?}");
    assert_eq!(refused.diagnostics[0].location, "/declared_ends/ticks");
}

#[test]
fn comparison_aligns_insertions_and_pages_every_difference() {
    let notes: Events = (0..400)
        .flat_map(|i| [(i * 10, on(0, 60, 100)), (i * 10 + 5, off(0, 60))])
        .collect();
    let reference = file(vec![notes.clone()], 4000);
    let identical = tunemend::midi::compare_midi(&reference, &reference);
    let keys: Vec<_> = identical.as_object().unwrap().keys().cloned().collect();
    assert_eq!(
        keys,
        [
            "aligned_event_differences",
            "byte_identical",
            "candidate_sha256",
            "comparable",
            "container_identical",
            "differences",
            "events_identical",
            "reference_sha256",
            "scope"
        ],
        "identical inputs keep the earlier report fields"
    );
    // One inserted control, one changed velocity and one moved release.
    let mut edited = notes.clone();
    edited.insert(100, (500, cc(0, 7, 90)));
    edited[2] = (10, on(0, 60, 50));
    edited[800] = (3998, off(0, 60));
    let candidate = file(vec![edited], 4000);
    let comparison = tunemend::midi::compare_midi(&reference, &candidate);
    assert_eq!(comparison["aligned_event_differences"], 3, "{comparison:#}");
    let changes: Vec<_> = comparison["differences"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["change"].as_str())
        .collect();
    assert_eq!(changes, ["changed", "inserted", "moved"]);
    let inserted = &comparison["differences"][2];
    assert_eq!(inserted["candidate_location"], "/tracks/0/events/100");
    assert!(
        inserted["candidate"]
            .as_str()
            .unwrap()
            .starts_with("MIDI channel 1:")
    );
    // A transposition of every note is 800 differences, reachable by paging.
    let transposed: Events = (0..400)
        .flat_map(|i| [(i * 10, on(0, 62, 100)), (i * 10 + 5, off(0, 62))])
        .collect();
    let transposed = file(vec![transposed], 4000);
    let mut offset = 0;
    let mut seen = 0;
    loop {
        let page = tunemend::midi::compare_midi_page(&reference, &transposed, offset, 300);
        seen += page["differences"].as_array().unwrap().len();
        match page["next_cursor"].as_u64() {
            Some(next) => offset = next as usize,
            None => break,
        }
    }
    assert_eq!(seen, 800);
}

/// Interleaved float samples of a WAV file's data chunk.
fn wav_samples(path: &str) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap();
    let at = bytes.windows(4).position(|w| w == b"data").unwrap();
    bytes[at + 8..]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

#[test]
fn retained_notes_are_listed_as_sounding_and_every_render_releases_them_at_the_end() {
    // Key 64 starts at tick 240 and is never released; the song ends at 960.
    let bytes = file(
        vec![vec![
            (0, on(0, 60, 100)),
            (240, on(0, 64, 90)),
            (480, off(0, 60)),
        ]],
        960,
    );
    let mut work = Work::new("held", &bytes);
    let project = work.service.call(
        "render_batch",
        json!({"job_id": "project", "project": "song", "sample_rate": 16000, "format": "float32"}),
    );
    assert_eq!(project.status, Status::Ok, "{project:?}");
    let loaded = work
        .service
        .call("load_midi", json!({"job_id": "load", "path": "song.mid"}));
    let source = work.service.call(
        "render_preview",
        json!({"job_id": "source", "source": loaded.result["source"], "destination": "out/source", "sample_rate": 16000,
            "layout": {"kind": "full", "loop_passes": 1}}),
    );
    assert_eq!(source.status, Status::Ok, "{source:?}");
    // The project render now releases the held note at the content end, as
    // source playback does, instead of ringing through the whole tail.
    let project_wav = wav_samples(project.artifacts[0].path.as_str());
    let source_wav = wav_samples(work.root.join("out/source/preview.wav").to_str().unwrap());
    assert_eq!(project_wav, source_wav);
    assert!(project_wav.len() < 2 * (16000 + 3 * 16000));

    let presented = work.service.call(
        "present_review",
        json!({"job_id": "show", "project": "song", "renders": [{"render": "render-1", "label": "draft"}]}),
    );
    assert_eq!(presented.status, Status::Ok, "{presented:?}");
    let marked = work.service.call(
        "record_feedback",
        json!({"job_id": "mark", "project": "song", "presentation": "review-1",
            "marks": [{"render": "render-1", "start_sample": 6000}], "text": "busy here"}),
    );
    assert_eq!(marked.status, Status::Ok, "{marked:?}");
    let feedback = work
        .service
        .call("read_feedback", json!({"project": "song"}));
    let sounding = &feedback.result["feedback"][0]["marks"][0]["start"]["sounding"];
    assert_eq!(
        sounding,
        &json!([
            {"id": "n1", "voice": "track0-ch1", "pitch": 60, "start": 0, "duration": 480},
            {"source_event": "/tracks/0/events/1", "voice": "track0-ch1", "pitch": 64, "start": 240,
                "retained": "never_released", "sounds_until": "content end"}
        ])
    );
}

#[test]
fn declared_ends_apply_to_chosen_patterns_and_export_can_write_them() {
    // Key 72 is never released; key 36 has a zero-length pair at 400 and a
    // later note 500..600, so a track-end release for the pair would pair
    // the later note's release with it instead.
    let bytes = file(
        vec![vec![
            (300, on(0, 72, 90)),
            (400, on(0, 36, 80)),
            (400, off(0, 36)),
            (500, on(0, 36, 70)),
            (600, off(0, 36)),
        ]],
        960,
    );
    let (mut work, imported) = Work::importing(
        "apply-to",
        &bytes,
        json!({"declared_ends": {"end": "track_end", "apply_to": ["never_released"]}}),
    );
    let declared = &imported.result["declared_ends"];
    assert_eq!(declared["apply_to"], json!(["never_released"]));
    assert_eq!(declared["notes"].as_array().unwrap().len(), 1);
    assert_eq!(declared["notes"][0]["from"], "never_released");
    assert!(
        declared["not_completed"][0]["message"]
            .as_str()
            .unwrap()
            .contains("does not apply to zero-length pairs")
    );
    // Both patterns: the zero-length pair cannot take a track-end release.
    let both = tunemend::midi::read_smf_declaring(
        &bytes,
        Some(serde_json::from_value(json!({"end": "track_end"})).unwrap()),
    )
    .unwrap();
    assert_eq!(both.composition.notes.len(), 2);
    assert!(
        both.retained
            .iter()
            .any(|r| r.location == "/tracks/0/events/1"
                && r.message.contains("first-in-first-out pairing"))
    );
    // Exact bytes by default; an explicit option writes every declared end.
    assert_eq!(work.export("plain"), bytes);
    let written = work.service.call(
        "export_project",
        json!({"job_id": "export-written", "project": "song", "destination": "out/written",
            "render_audio": "none", "write_declared_ends": true}),
    );
    assert_eq!(written.status, Status::Ok, "{written:?}");
    let report = &written.result["declared_ends_written"];
    assert_eq!(report.as_array().unwrap().len(), 1);
    assert_eq!(report[0]["release_written_at"], 960);
    let output = std::fs::read(work.root.join("out/written/song.mid")).unwrap();
    assert!(events(&output)[0].contains(&(960, off(0, 72))));
    // An empty or repeated scope is refused.
    let refused = work.service.call(
        "import_midi",
        json!({"job_id": "empty", "project": "empty", "path": "song.mid",
            "declared_ends": {"end": "next_onset", "apply_to": []}}),
    );
    assert_eq!(refused.diagnostics[0].location, "/declared_ends/apply_to");
}

#[test]
fn project_playback_stops_declared_notes_at_their_declared_end() {
    let bytes = file(
        vec![vec![
            (0, on(0, 60, 100)),
            (240, off(0, 60)),
            (300, on(0, 72, 90)),
        ]],
        960,
    );
    let released = file(
        vec![vec![
            (0, on(0, 60, 100)),
            (240, off(0, 60)),
            (300, on(0, 72, 90)),
            (400, off(0, 72)),
        ]],
        960,
    );
    let (mut work, _) = Work::importing(
        "declared-playback",
        &bytes,
        json!({"declared_ends": {"end": "fixed", "ticks": 100}}),
    );
    let project = work.service.call(
        "render_batch",
        json!({"job_id": "project", "project": "song", "sample_rate": 16000, "format": "float32"}),
    );
    assert_eq!(project.status, Status::Ok, "{project:?}");
    std::fs::write(work.root.join("released.mid"), &released).unwrap();
    let loaded = work.service.call(
        "load_midi",
        json!({"job_id": "load", "path": "released.mid"}),
    );
    let source = work.service.call(
        "render_preview",
        json!({"job_id": "source", "source": loaded.result["source"], "destination": "out/released", "sample_rate": 16000,
            "layout": {"kind": "full", "loop_passes": 1}}),
    );
    assert_eq!(source.status, Status::Ok, "{source:?}");
    assert_eq!(
        wav_samples(project.artifacts[0].path.as_str()),
        wav_samples(work.root.join("out/released/preview.wav").to_str().unwrap())
    );
    // The delivered MIDI still keeps the original bytes by default.
    assert_eq!(work.export("plain"), bytes);
}

#[test]
fn a_voice_without_its_own_lanes_plays_with_its_channels_program_and_volume() {
    // Track 1 sets channel 1's program and volume; track 2 plays on channel 1.
    let program = midi(
        0,
        MidiMessage::ProgramChange {
            program: u7::new(40),
        },
    );
    let bytes = file(
        vec![
            vec![
                (0, program),
                (0, cc(0, 7, 60)),
                (0, on(0, 72, 90)),
                (240, off(0, 72)),
            ],
            vec![(0, on(0, 60, 100)), (480, off(0, 60))],
        ],
        960,
    );
    let mut work = Work::new("channel-state", &bytes);
    let stems = work.service.call(
        "render_batch",
        json!({"job_id": "stems", "project": "song", "sample_rate": 16000, "format": "float32", "mixdown": false,
            "stems": {"by": "group", "groups": [{"id": "second", "voices": ["track1-ch1"]}]}}),
    );
    assert_eq!(stems.status, Status::Ok, "{stems:?}");
    // The same notes on one receiver with the first track's controls only.
    let alone = file(
        vec![
            vec![(0, program), (0, cc(0, 7, 60))],
            vec![(0, on(0, 60, 100)), (480, off(0, 60))],
        ],
        960,
    );
    std::fs::write(work.root.join("alone.mid"), &alone).unwrap();
    let loaded = work
        .service
        .call("load_midi", json!({"job_id": "load", "path": "alone.mid"}));
    let source = work.service.call(
        "render_preview",
        json!({"job_id": "source", "source": loaded.result["source"], "destination": "out/alone", "sample_rate": 16000,
            "layout": {"kind": "full", "loop_passes": 1}}),
    );
    assert_eq!(source.status, Status::Ok, "{source:?}");
    assert_eq!(
        wav_samples(stems.artifacts[0].path.as_str()),
        wav_samples(work.root.join("out/alone/preview.wav").to_str().unwrap())
    );
}

#[test]
fn nrpn_data_stays_exact_through_note_edits_without_becoming_an_rpn_lane() {
    let bytes = file(
        vec![vec![
            (0, cc(0, 101, 0)),
            (0, cc(0, 100, 0)),
            (0, cc(0, 6, 2)),
            (0, cc(0, 99, 20)),
            (0, cc(0, 98, 1)),
            (0, cc(0, 6, 100)),
            (0, cc(0, 38, 12)),
            (0, on(0, 60, 90)),
            (240, off(0, 60)),
        ]],
        480,
    );
    let imported = read_smf_preserving(&bytes).unwrap();
    let ranges: Vec<_> = imported
        .composition
        .control_lanes
        .iter()
        .filter(|l| l.kind == tunemend::composition::ControlKind::PitchBendRange)
        .collect();
    assert_eq!(ranges.len(), 1);
    assert_eq!(ranges[0].points.len(), 1);
    let mut work = Work::new("nrpn-preservation", &bytes);
    assert_eq!(work.export("unchanged"), bytes);
    let note = &imported.composition.notes[0];
    let response = work.edit(
        "velocity",
        "r1",
        json!({"operations":[
            {"op":"update_notes", "notes":[{"id":note.id,"velocity":80}]}
        ]}),
    );
    assert_eq!(response.status, Status::Ok, "{response:?}");
    let changed = work.export("changed");
    let controls = |b: &[u8]| {
        events(b)
            .into_iter()
            .flatten()
            .filter_map(|(tick, e)| match e {
                TrackEventKind::Midi {
                    message: MidiMessage::Controller { controller, value },
                    ..
                } => Some((tick, controller.as_int(), value.as_int())),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(controls(&bytes), controls(&changed));
    // Returning to an unsupported RPN must still refuse, not inherit NRPN retention.
    let unsupported = file(
        vec![vec![
            (0, cc(0, 99, 1)),
            (0, cc(0, 101, 0)),
            (0, cc(0, 100, 1)),
            (0, cc(0, 6, 10)),
            (0, on(0, 60, 90)),
            (240, off(0, 60)),
        ]],
        480,
    );
    assert!(read_smf_preserving(&unsupported).is_err());
}

#[test]
fn legacy_and_current_declared_projects_resume_without_reinterpreting_notes() {
    let bytes = file(
        vec![vec![
            (0, on(0, 60, 90)),
            (0, off(0, 60)),
            (100, on(0, 60, 80)),
            (200, off(0, 60)),
        ]],
        480,
    );
    let (mut work, _) = Work::importing(
        "legacy-declared",
        &bytes,
        json!({"declared_ends":{"end":"track_end"}}),
    );
    assert_eq!(work.export("current"), bytes);
    let mut bundle: Value = serde_json::from_slice(
        &std::fs::read(work.root.join("out/current/song.project.json")).unwrap(),
    )
    .unwrap();
    let current_notes = bundle["revisions"][0]["composition"]["notes"].clone();
    assert_eq!(current_notes.as_array().unwrap().len(), 1);
    // Reconstruct the pre-FIFO-check root: the zero-length first note was
    // declared through track end even though a later same-key note ends sooner.
    let mut second = current_notes[0].clone();
    second["id"] = json!("n2");
    let mut first = second.clone();
    first["id"] = json!("n1");
    first["start"] = json!(0);
    first["duration"] = json!(480);
    first["velocity"] = json!(90);
    first.as_object_mut().unwrap().remove("release_velocity");
    bundle["revisions"][0]["composition"]["notes"] = json!([first, second]);
    bundle["project"]["midi_source"]
        .as_object_mut()
        .unwrap()
        .remove("legacy_declared_ends");
    let path = work.root.join("legacy.project.json");
    std::fs::write(&path, serde_json::to_vec(&bundle).unwrap()).unwrap();
    let mut resumed = Service::new(work.root.join("legacy"));
    let imported = resumed.call(
        "project",
        json!({"operation":"import_project","arguments":{"job_id":"resume","path":path}}),
    );
    assert_eq!(imported.status, Status::Ok, "{imported:?}");
    let export = resumed.call(
        "export_project",
        json!({"job_id":"export","project":"song","destination":"out","render_audio":"none"}),
    );
    assert_eq!(export.status, Status::Ok, "{export:?}");
    assert_eq!(
        std::fs::read(work.root.join("legacy/out/song.mid")).unwrap(),
        bytes
    );
    let delivered: Value = serde_json::from_slice(
        &std::fs::read(work.root.join("legacy/out/song.project.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        delivered["project"]["midi_source"]["legacy_declared_ends"],
        true
    );
    assert_eq!(
        delivered["revisions"][0]["composition"]["notes"],
        bundle["revisions"][0]["composition"]["notes"]
    );
    // Writing the unsafe declared release must still refuse rather than alter
    // which later note the source NoteOff releases.
    let refused = resumed.call("export_project",json!({"job_id":"unsafe","project":"song","destination":"unsafe","render_audio":"none","write_declared_ends":true}));
    assert_eq!(refused.status, Status::Blocked);
    assert!(!work.root.join("legacy/unsafe").exists());
    // Unmarked bundles made after the fix retain their one-note projection.
    bundle["revisions"][0]["composition"]["notes"] = current_notes;
    std::fs::write(&path, serde_json::to_vec(&bundle).unwrap()).unwrap();
    let mut current = Service::new(work.root.join("modern"));
    assert_eq!(
        current
            .call("import_project", json!({"job_id":"resume","path":path}))
            .status,
        Status::Ok
    );
    let export = current.call(
        "export_project",
        json!({"job_id":"export","project":"song","destination":"out","render_audio":"none"}),
    );
    assert_eq!(export.status, Status::Ok, "{export:?}");
    assert_eq!(
        std::fs::read(work.root.join("modern/out/song.mid")).unwrap(),
        bytes
    );
}

fn port(value: u8) -> TrackEventKind<'static> {
    TrackEventKind::Meta(MetaMessage::MidiPort(u7::new(value)))
}

#[test]
fn port_identity_separates_edit_protections_and_survives_delivery() {
    let bytes = file(
        vec![
            vec![
                (0, port(9)),
                (0, cc(0, 7, 100)),
                (0, on(0, 60, 90)),
                (480, off(0, 60)),
            ],
            vec![
                (0, port(10)),
                (0, cc(0, 7, 60)),
                (0, on(0, 60, 90)),
                (480, off(0, 60)),
            ],
            vec![(0, port(9)), (0, on(0, 64, 90)), (480, off(0, 64))],
        ],
        960,
    );
    let mut work = Work::new("port-protection", &bytes);
    assert_eq!(work.export("original"), bytes);
    let lane = json!({"op":"set_control_points","voice":"track0-port9-ch1","kind":"volume","points":[{"at":0,"value":80}]});
    let blocked = work.edit(
        "narrow",
        "r1",
        json!({"scope":{"voices":["track0-port9-ch1"]},"operations":[lane]}),
    );
    assert!(codes(&blocked).contains(&Code::OutOfScope));
    assert_eq!(work.head(), "r1");
    let protected = work.edit(
        "protected",
        "r1",
        json!({"protections":[{"voice":"track2-port9-ch1"}],"operations":[lane]}),
    );
    assert!(codes(&protected).contains(&Code::ProtectedChange));
    let edited = work.edit("edit","r1",json!({"scope":{"voices":["track0-port9-ch1","track2-port9-ch1"]},"protections":[{"voice":"track1-port10-ch1"}],"operations":[lane]}));
    assert_eq!(edited.status, Status::Ok, "{edited:?}");
    let output = work.export("delivery");
    let (before, after) = (events(&bytes), events(&output));
    assert_eq!(after[1..], before[1..]);
    assert_eq!(after[0][0], before[0][0]);
    assert_eq!(after[0][1], (0, cc(0, 7, 80)));
    assert_eq!(after[0][2..], before[0][2..]);
    assert_eq!(
        read_smf_preserving(&output).unwrap().assignments,
        read_smf_preserving(&bytes).unwrap().assignments
    );
    let mut resumed = Service::new(work.root.join("resumed"));
    let restored = resumed.call(
        "import_project",
        json!({"job_id":"resume","path":work.root.join("out/delivery/song.project.json")}),
    );
    assert_eq!(restored.status, Status::Ok, "{restored:?}");
    let exported = resumed.call(
        "export_project",
        json!({"job_id":"again","project":"song","destination":"again","render_audio":"none"}),
    );
    assert_eq!(exported.status, Status::Ok, "{exported:?}");
    assert_eq!(
        std::fs::read(work.root.join("resumed/again/song.mid")).unwrap(),
        output
    );
}

#[test]
fn mid_track_port_changes_keep_bindings_and_refuse_implicit_rerouting() {
    let bytes = file(
        vec![vec![
            (0, port(1)),
            (0, on(0, 60, 90)),
            (120, off(0, 60)),
            (240, port(2)),
            (240, on(0, 60, 80)),
            (360, off(0, 60)),
        ]],
        480,
    );
    let imported = read_smf_preserving(&bytes).unwrap();
    assert_eq!(imported.composition.notes[0].voice, "track0-port1-ch1");
    assert_eq!(imported.composition.notes[1].voice, "track0-port2-ch1");
    let mut work = Work::new("port-switch-edit", &bytes);
    let bad = work.edit(
        "cross-port",
        "r1",
        json!({"operations":[{"op":"update_notes","notes":[{"id":"n1","start":250}]}]}),
    );
    assert_eq!(bad.status, Status::Blocked, "{bad:?}");
    assert_eq!(work.head(), "r1");
    assert_eq!(work.export("unchanged"), bytes);
    let good = work.edit(
        "velocity",
        "r1",
        json!({"operations":[{"op":"update_notes","notes":[{"id":"n2","velocity":50}]}]}),
    );
    assert_eq!(good.status, Status::Ok, "{good:?}");
    let output = work.export("edited");
    let (before, after) = (events(&bytes), events(&output));
    assert_eq!(before[0][..4], after[0][..4]);
    assert_eq!(after[0][4], (240, on(0, 60, 50)));
    assert_eq!(before[0][5..], after[0][5..]);
}

#[test]
fn port_stems_keep_controls_and_effects_on_their_own_receiver() {
    let program = midi(
        0,
        MidiMessage::ProgramChange {
            program: u7::new(40),
        },
    );
    let bytes = file(
        vec![
            vec![
                (0, program),
                (0, cc(0, 7, 20)),
                (0, cc(0, 91, 100)),
                (0, on(0, 60, 90)),
                (480, off(0, 60)),
            ],
            vec![(0, port(5)), (0, on(0, 60, 100)), (480, off(0, 60))],
            // A controller-only track has no editable voice, but still reaches port 5.
            vec![(0, port(5)), (0, cc(0, 64, 127)), (720, cc(0, 64, 0))],
        ],
        960,
    );
    let mut work = Work::new("port-stems", &bytes);
    let stems=work.service.call("render_batch",json!({"job_id":"stems","project":"song","sample_rate":16000,"format":"float32","mixdown":false,"stems":{"by":"group","groups":[{"id":"second","voices":["track1-port5-ch1"]}]}}));
    assert_eq!(stems.status, Status::Ok, "{stems:?}");
    let alone = file(
        vec![
            vec![(0, on(0, 60, 100)), (480, off(0, 60))],
            vec![(0, cc(0, 64, 127)), (720, cc(0, 64, 0))],
        ],
        960,
    );
    std::fs::write(work.root.join("alone.mid"), &alone).unwrap();
    let loaded = work
        .service
        .call("load_midi", json!({"job_id":"load","path":"alone.mid"}));
    let source=work.service.call("render_preview",json!({"job_id":"source","source":loaded.result["source"],"destination":"alone","sample_rate":16000,"layout":{"kind":"full","loop_passes":1}}));
    assert_eq!(source.status, Status::Ok, "{source:?}");
    assert_eq!(
        wav_samples(stems.artifacts[0].path.as_str()),
        wav_samples(work.root.join("alone/preview.wav").to_str().unwrap())
    );
}

#[test]
fn rpn_selection_does_not_leak_between_ports() {
    let first = vec![
        (0, cc(0, 101, 0)),
        (0, cc(0, 100, 0)),
        (0, cc(0, 6, 12)),
        (0, on(0, 60, 90)),
        (240, off(0, 60)),
    ];
    let unselected = file(
        vec![
            first.clone(),
            vec![
                (0, port(1)),
                (0, cc(0, 6, 3)),
                (0, on(0, 60, 90)),
                (240, off(0, 60)),
            ],
        ],
        480,
    );
    let refused = read_smf_preserving(&unselected).unwrap_err();
    assert!(refused.iter().any(|d| d.location == "/tracks/1/events/1"));
    let nrpn = file(
        vec![
            first,
            vec![
                (0, port(1)),
                (0, cc(0, 99, 1)),
                (0, cc(0, 98, 2)),
                (0, cc(0, 6, 3)),
                (0, on(0, 60, 90)),
                (240, off(0, 60)),
            ],
        ],
        480,
    );
    let imported = read_smf_preserving(&nrpn).unwrap();
    assert_eq!(imported.composition.control_lanes.len(), 1);
    assert_eq!(imported.composition.control_lanes[0].voice, "track0-ch1");
    assert_eq!(imported.composition.control_lanes[0].points[0].value, 12);
}

#[test]
fn same_tick_control_edits_select_the_correct_port() {
    let bytes = file(
        vec![vec![
            (0, port(1)),
            (0, cc(0, 7, 100)),
            (0, on(0, 60, 90)),
            (0, port(2)),
            (0, cc(0, 7, 40)),
            (0, on(0, 60, 80)),
            (240, off(0, 60)),
            (240, port(1)),
            (240, off(0, 60)),
        ]],
        480,
    );
    let mut work = Work::new("port-same-tick-controls", &bytes);
    let edited=work.edit("volume","r1",json!({"protections":[{"voice":"track0-port1-ch1"}],"operations":[{"op":"set_control_points","voice":"track0-port2-ch1","kind":"volume","points":[{"at":0,"value":50}]}]}));
    assert_eq!(edited.status, Status::Ok, "{edited:?}");
    let output = work.export("edited");
    let (before, after) = (events(&bytes), events(&output));
    assert_eq!(before[0][..4], after[0][..4]);
    assert_eq!(after[0][4], (0, cc(0, 7, 50)));
    assert_eq!(before[0][5..], after[0][5..]);
}
