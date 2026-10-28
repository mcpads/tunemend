//! Play retained events without requiring a note-editing projection.
//!
//! Every MIDI port is its own reference receiver, with independent channel,
//! program, controller, pedal and note state. An SMF type 2 sequence plays
//! alone, on its own clock from its own tick zero.
use super::*;
use crate::diagnostic::{Diagnostic, DiagnosticCode as Code};
use midly::{Format, MetaMessage, MidiMessage, Smf, Timing, TrackEventKind};
use serde::Serialize;
use serde_json::{Value, json};

/// Distinct MIDI ports one source render may play as separate receivers.
pub const MAX_SOURCE_RECEIVERS: usize = 16;
/// Type 2 sequences listed individually by [`midi_source_playback`].
const LISTED_SEQUENCES: usize = 256;

/// The reference receiver of one MIDI port and the stream routed to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceReceiver {
    pub port: u8,
    /// Tracks that send at least one event to this port, in original order.
    pub tracks: Vec<usize>,
    pub channel_events: usize,
    pub sysex_events: usize,
    /// Reverb and chorus run when this receiver's stream contains CC91 or CC93.
    #[serde(skip)]
    effects: bool,
}

/// Source playback audio and the routing that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceRender {
    pub audio: RenderedAudio,
    /// In ascending port order; their outputs sum in this order.
    pub receivers: Vec<SourceReceiver>,
    /// The type 2 sequence (track index) that played.
    pub sequence: Option<usize>,
}

/// [`render_midi_sequence`] for SMF 0/1 and RMID, which hold one sequence.
pub fn render_midi_source(
    bytes: &[u8],
    layout: &RenderLayout,
    sound_font: &[u8],
    sample_rate: u32,
) -> Result<RenderedAudio, RenderError> {
    render_midi_sequence(bytes, None, layout, sound_font, sample_rate).map(|r| r.audio)
}

/// Plays the retained events with the original track order at equal ticks.
/// `sequence` selects one type 2 track and must be absent for type 0/1.
/// Excerpts preroll from zero. The transport releases held notes and pedals
/// on every receiver at the requested end; this adds no events to the source.
pub fn render_midi_sequence(
    bytes: &[u8],
    sequence: Option<usize>,
    layout: &RenderLayout,
    sound_font: &[u8],
    sample_rate: u32,
) -> Result<SourceRender, RenderError> {
    let SourcePlayback {
        scheduled,
        receivers,
        crop,
        content,
        ..
    } = prepare(bytes, sequence, layout, sample_rate)?;
    let font = load_font(sound_font)?;
    let mut synths = receivers
        .iter()
        .map(|receiver| synthesizer_for(&font, sample_rate, receiver.effects))
        .collect::<Result<Vec<_>, _>>()?;
    let mut mix = Mixdown::default();
    for (sample, receiver, channel, message) in scheduled {
        mix.advance(&mut synths, sample);
        send(&mut synths[receiver], channel, message);
    }
    Ok(SourceRender {
        audio: mix.finish(synths, content, sample_rate, crop),
        receivers,
        sequence,
    })
}

/// Receivers advance together. The first renders straight into the sum and
/// the others add theirs in port order, so one receiver is its own output.
#[derive(Default)]
pub(super) struct Mixdown {
    left: Vec<f32>,
    right: Vec<f32>,
    scratch_left: Vec<f32>,
    scratch_right: Vec<f32>,
}

impl Mixdown {
    /// Release all receivers at the content end and trim their summed tail.
    pub(super) fn finish(
        mut self,
        mut synths: Vec<Synthesizer>,
        content: u64,
        sample_rate: u32,
        crop: usize,
    ) -> RenderedAudio {
        self.advance(&mut synths, content);
        for synth in &mut synths {
            release_every_note(synth);
        }
        // The tail ends when the sum of all receivers decays, as for one receiver.
        let block = (sample_rate / 100).max(1) as u64;
        let max_tail = u64::from(MAX_TAIL_SECONDS) * u64::from(sample_rate);
        let mut tail_truncated = true;
        while (self.left.len() as u64) < content + max_tail {
            let start = self.left.len();
            self.advance(&mut synths, start as u64 + block);
            let peak = self.left[start..]
                .iter()
                .chain(&self.right[start..])
                .fold(0.0f32, |peak, s| peak.max(s.abs()));
            if peak < SILENCE_PEAK {
                tail_truncated = false;
                break;
            }
        }
        let Mixdown {
            mut left,
            mut right,
            ..
        } = self;
        left.drain(..crop);
        right.drain(..crop);
        RenderedAudio {
            sample_rate,
            left,
            right,
            content_samples: content - crop as u64,
            tail_truncated,
            stems: Vec::new(),
            pre_master_mix: None,
        }
    }

    pub(super) fn advance(&mut self, synths: &mut [Synthesizer], sample: u64) {
        let start = self.left.len();
        let missing = sample.saturating_sub(start as u64) as usize;
        let Some((first, others)) = synths.split_first_mut() else {
            return;
        };
        if missing == 0 {
            return;
        }
        render_until(first, &mut self.left, &mut self.right, sample);
        for synth in others {
            self.scratch_left.clear();
            self.scratch_left.resize(missing, 0.0);
            self.scratch_right.clear();
            self.scratch_right.resize(missing, 0.0);
            synth.render(&mut self.scratch_left, &mut self.scratch_right);
            for (sum, value) in self.left[start..].iter_mut().zip(&self.scratch_left) {
                *sum += value;
            }
            for (sum, value) in self.right[start..].iter_mut().zip(&self.scratch_right) {
                *sum += value;
            }
        }
    }
}

struct SourcePlayback {
    /// Sample, receiver index, channel and message, in playing order.
    scheduled: Vec<(u64, usize, u8, ChannelMessage)>,
    receivers: Vec<SourceReceiver>,
    crop: usize,
    content: u64,
    end: u64,
    /// Where each tempo segment starts: tick, seconds there, seconds per tick.
    clock: Vec<(u64, Rational, Rational)>,
    /// Tick, track and event index of each scheduled message, in the same order.
    origins: Vec<(u64, usize, usize)>,
}

pub fn midi_source_preview_problem(bytes: &[u8]) -> Option<String> {
    prepare(bytes, None, &RenderLayout::Full { loop_passes: 1 }, 44100)
        .err()
        .map(|e| e.to_string())
}

/// What source playback does with these bytes, without rendering: a full-pass
/// preflight at 44100 Hz with the diagnostics a render returns, the receivers
/// it would play, and for type 2 the same for each selectable sequence.
pub fn midi_source_playback(bytes: &[u8]) -> Value {
    let describe = |sequence| match prepare(
        bytes,
        sequence,
        &RenderLayout::Full { loop_passes: 1 },
        44100,
    ) {
        Ok(plan) => json!({"available": true, "receivers": plan.receivers}),
        Err(error) => {
            let diagnostics = match &error {
                RenderError::MidiEvents(diagnostics) => diagnostics.clone(),
                other => vec![Diagnostic {
                    code: Code::UnsupportedMidi,
                    location: "/source".into(),
                    message: other.to_string(),
                }],
            };
            json!({"available": false, "problem": error.to_string(),
                    "diagnostic_count": diagnostics.len(),
                    "diagnostics": diagnostics.iter().take(20).collect::<Vec<_>>()})
        }
    };
    let mut summary = describe(None);
    if let Some(timed) = has_timed_content(bytes) {
        summary["timed_content"] = json!(timed);
    }
    if let Some(count) = type_2_sequences(bytes) {
        summary["sequence_count"] = json!(count);
        summary["sequences"] = (0..count.min(LISTED_SEQUENCES))
            .map(|sequence| {
                let mut entry = describe(Some(sequence));
                entry["sequence"] = json!(sequence);
                entry
            })
            .collect();
    }
    summary
}

/// Where a sample of a source render lies in the source: its exact tick on the
/// render's own clock (tempo map or SMPTE, excerpt offset) and the retained
/// note-on events still held there on each receiver, by `inspect_midi` event
/// ID. Held means not yet released by NoteOff, a channel-mode message or the
/// pedals, following the reference receiver; decaying releases are not listed.
/// In the tail, `held_at_end` lists the notes the transport released.
pub fn locate_source_sample(
    bytes: &[u8],
    sequence: Option<usize>,
    layout: &RenderLayout,
    sample_rate: u32,
    sample: u64,
) -> Result<Value, RenderError> {
    let plan = prepare(bytes, sequence, layout, sample_rate)?;
    let content = plan.content - plan.crop as u64;
    let rate = i128::from(sample_rate);
    let render_seconds = Rational::new(i128::from(sample), rate);
    let ports: Vec<u8> = plan.receivers.iter().map(|r| r.port).collect();
    if sample >= content {
        return Ok(json!({
            "render_seconds": seconds(render_seconds),
            "in_tail": true,
            "seconds_after_content": seconds(Rational::new(i128::from(sample - content), rate)),
            "end_tick": plan.end,
            "held_at_end": held_notes(&plan, &ports, plan.end),
        }));
    }
    // The render's sample 0 is the first sample at or after the excerpt start.
    let at = Rational::new(i128::from(plan.crop as u64 + sample), rate);
    let &(start_tick, start_seconds, per_tick) = plan
        .clock
        .iter()
        .rev()
        .find(|(_, seconds, _)| *seconds <= at)
        .expect("the clock starts at zero seconds");
    let tick = Rational::from_integer(i128::from(start_tick)) + (at - start_seconds) / per_tick;
    let whole = tick.floor().to_integer() as u64;
    Ok(json!({
        "render_seconds": seconds(render_seconds),
        "in_tail": false,
        "source_seconds": seconds(at),
        "source_tick": whole,
        "source_tick_exact": format!("{}/{}", tick.numer(), tick.denom()),
        "held": held_notes(&plan, &ports, whole),
    }))
}

fn seconds(value: Rational) -> f64 {
    *value.numer() as f64 / *value.denom() as f64
}

/// Notes held after every scheduled message at or before `tick`.
fn held_notes(plan: &SourcePlayback, ports: &[u8], tick: u64) -> Vec<Value> {
    struct Held {
        receiver: usize,
        channel: u8,
        key: u8,
        velocity: u8,
        origin: (u64, usize, usize),
        key_down: bool,
        latched: bool,
    }
    let mut held: Vec<Held> = Vec::new();
    // Sustain and sostenuto per receiver and channel.
    let mut pedals: BTreeMap<(usize, u8), (bool, bool)> = BTreeMap::new();
    for ((_, receiver, channel, message), origin) in plan.scheduled.iter().zip(&plan.origins) {
        if origin.0 > tick {
            break;
        }
        let (receiver, channel) = (*receiver, *channel);
        let pedal = pedals.entry((receiver, channel)).or_default();
        let mine = |h: &Held| h.receiver == receiver && h.channel == channel;
        match *message {
            ChannelMessage::NoteOn { key, velocity } => held.push(Held {
                receiver,
                channel,
                key,
                velocity,
                origin: *origin,
                key_down: true,
                latched: false,
            }),
            ChannelMessage::NoteOff { key, .. } => held
                .iter_mut()
                .filter(|h| mine(h) && h.key == key)
                .for_each(|h| h.key_down = false),
            ChannelMessage::Controller { controller, value } => match controller {
                64 => pedal.0 = value >= 64,
                66 => {
                    let down = value >= 64;
                    if down && !pedal.1 {
                        held.iter_mut()
                            .filter(|h| mine(h) && h.key_down)
                            .for_each(|h| h.latched = true);
                    }
                    if !down {
                        held.iter_mut()
                            .filter(|h| mine(h))
                            .for_each(|h| h.latched = false);
                    }
                    pedal.1 = down;
                }
                120 => held.retain(|h| !mine(h)),
                121 => *pedal = (false, false),
                123..=127 => held
                    .iter_mut()
                    .filter(|h| mine(h))
                    .for_each(|h| h.key_down = false),
                _ => {}
            },
            _ => {}
        }
        let (sustain, sostenuto) = *pedal;
        held.retain(|h| !mine(h) || h.key_down || sustain || (h.latched && sostenuto));
    }
    held.iter()
        .map(|h| {
            let (sustain, _) = pedals[&(h.receiver, h.channel)];
            json!({"event": format!("t{}:e{}", h.origin.1, h.origin.2), "tick": h.origin.0,
                "port": ports[h.receiver], "channel": h.channel, "midi_channel": h.channel + 1, "key": h.key, "velocity": h.velocity,
                "held_by": if h.key_down { "key" } else if sustain { "sustain" } else { "sostenuto" }})
        })
        .collect()
}

/// Whether any track of the source advances past tick 0. Without timed
/// content nothing can be played or placed in time, whatever the events are.
pub fn has_timed_content(bytes: &[u8]) -> Option<bool> {
    let container = crate::midi::source_layout(bytes).ok()?;
    let smf = Smf::parse(&bytes[container.smf_offset..container.smf_offset + container.smf_length])
        .ok()?;
    Some(
        smf.tracks
            .iter()
            .any(|track| track.iter().any(|event| event.delta.as_int() > 0)),
    )
}

/// The number of independent sequences when the source is SMF type 2.
pub fn type_2_sequences(bytes: &[u8]) -> Option<usize> {
    let container = crate::midi::source_layout(bytes).ok()?;
    let smf = Smf::parse(&bytes[container.smf_offset..container.smf_offset + container.smf_length])
        .ok()?;
    (smf.header.format == Format::Sequential).then_some(smf.tracks.len())
}

fn unsupported(location: String, message: impl Into<String>) -> Diagnostic {
    Diagnostic {
        code: Code::UnsupportedMidi,
        location,
        message: message.into(),
    }
}

/// Routing of the selected tracks: the receivers in port order, and for each
/// selected track the receiver of every event (`None` for metadata).
type Routing = (Vec<SourceReceiver>, Vec<Vec<Option<usize>>>);

/// Each track's channel and SysEx events go to the port of the latest MIDI
/// Port meta event in that track, or port 0 before one. Distinct RP-019
/// device names are not modeled and refuse rather than merge. Events the
/// receiver cannot interpret refuse at their original location; a broadcast
/// GM1/GM2 reset is compatible only before any channel event on its receiver.
fn route(smf: &Smf<'_>, tracks: &[usize]) -> Result<Routing, Vec<Diagnostic>> {
    let mut receivers: BTreeMap<u8, SourceReceiver> = BTreeMap::new();
    let mut first_channel_event: BTreeMap<u8, (u64, usize, usize)> = BTreeMap::new();
    let mut device: Option<(&[u8], String)> = None;
    let mut ports = Vec::new();
    for &track in tracks {
        let mut port = 0;
        let mut name = None;
        let mut tick = 0u64;
        let mut track_ports = Vec::new();
        for (index, event) in smf.tracks[track].iter().enumerate() {
            tick += u64::from(event.delta.as_int());
            let location = format!("/tracks/{track}/events/{index}");
            match event.kind {
                TrackEventKind::Meta(MetaMessage::MidiPort(value)) => port = value.as_int(),
                TrackEventKind::Meta(MetaMessage::DeviceName(value)) => name = Some(value),
                TrackEventKind::Midi { .. }
                | TrackEventKind::SysEx(_)
                | TrackEventKind::Escape(_) => {
                    if let Some(name) = name {
                        match &device {
                            None => device = Some((name, location.clone())),
                            Some((first, at)) if *first != name => {
                                return Err(vec![unsupported(
                                    location,
                                    format!(
                                        "device names `{}` (first used at {at}) and `{}` name different devices; device-name routing is not modeled, so source playback refuses rather than merging them",
                                        String::from_utf8_lossy(first),
                                        String::from_utf8_lossy(name)
                                    ),
                                )]);
                            }
                            Some(_) => {}
                        }
                    }
                    if !receivers.contains_key(&port) && receivers.len() == MAX_SOURCE_RECEIVERS {
                        return Err(vec![unsupported(
                            location,
                            format!(
                                "this event addresses MIDI port {port}, beyond the {MAX_SOURCE_RECEIVERS} receivers source playback supports"
                            ),
                        )]);
                    }
                    let receiver = receivers.entry(port).or_insert_with(|| SourceReceiver {
                        port,
                        tracks: Vec::new(),
                        channel_events: 0,
                        sysex_events: 0,
                        effects: false,
                    });
                    if receiver.tracks.last() != Some(&track) {
                        receiver.tracks.push(track);
                    }
                    if let TrackEventKind::Midi { message, .. } = event.kind {
                        receiver.channel_events += 1;
                        receiver.effects |= matches!(message,
                            MidiMessage::Controller { controller, .. } if matches!(controller.as_int(), 91 | 93));
                        let first = first_channel_event
                            .entry(port)
                            .or_insert((tick, track, index));
                        *first = (*first).min((tick, track, index));
                    } else {
                        receiver.sysex_events += 1;
                    }
                    track_ports.push(Some(port));
                    continue;
                }
                _ => {}
            }
            track_ports.push(None);
        }
        ports.push(track_ports);
    }
    let mut diagnostics = Vec::new();
    for (&track, track_ports) in tracks.iter().zip(&ports) {
        let mut tick = 0u64;
        for (index, (event, port)) in smf.tracks[track].iter().zip(track_ports).enumerate() {
            tick += u64::from(event.delta.as_int());
            let Some(port) = port else { continue };
            let supported = match event.kind {
                TrackEventKind::SysEx(data) => {
                    crate::midi::gm_initialization(data)
                        && first_channel_event
                            .get(port)
                            .is_none_or(|first| (tick, track, index) < *first)
                }
                TrackEventKind::Midi { message, .. } => {
                    crate::midi::reference_channel_message(message)
                }
                _ => false,
            };
            if !supported {
                diagnostics.push(unsupported(format!("/tracks/{track}/events/{index}"), "this event is preserved in MIDI but has no verified reference-synth interpretation; preview is blocked"));
            }
        }
    }
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }
    if receivers.is_empty() {
        receivers.insert(
            0,
            SourceReceiver {
                port: 0,
                tracks: Vec::new(),
                channel_events: 0,
                sysex_events: 0,
                effects: false,
            },
        );
    }
    let index: BTreeMap<u8, usize> = receivers.keys().enumerate().map(|(i, p)| (*p, i)).collect();
    let routes = ports
        .into_iter()
        .map(|track| track.into_iter().map(|p| p.map(|p| index[&p])).collect())
        .collect();
    Ok((receivers.into_values().collect(), routes))
}

fn prepare(
    bytes: &[u8],
    sequence: Option<usize>,
    layout: &RenderLayout,
    sample_rate: u32,
) -> Result<SourcePlayback, RenderError> {
    let fail = |message: &str| RenderError::Midi(message.into());
    if !(16_000..=192_000).contains(&sample_rate) {
        return Err(fail("sample rate must be 16000–192000"));
    }
    let container = crate::midi::source_layout(bytes).map_err(RenderError::MidiEvents)?;
    let bytes = &bytes[container.smf_offset..container.smf_offset + container.smf_length];
    let Ok(smf) = Smf::parse(bytes) else {
        return Err(RenderError::MidiEvents(vec![unsupported(
            "/header".into(),
            "invalid Standard MIDI File",
        )]));
    };
    let count = smf.tracks.len();
    let tracks: Vec<usize> = match (smf.header.format, sequence) {
        (Format::Sequential, Some(selected)) if selected < count => vec![selected],
        (Format::Sequential, Some(selected)) => {
            return Err(fail(&format!(
                "sequence {selected} does not exist; this type 2 file holds {count} sequences"
            )));
        }
        (Format::Sequential, None) => {
            return Err(fail(
                "SMF type 2 contains independent sequences; select a sequence before rendering",
            ));
        }
        (_, Some(_)) => {
            return Err(fail(
                "only SMF type 2 holds independent sequences; omit sequence for type 0/1",
            ));
        }
        (_, None) => (0..count).collect(),
    };
    let (receivers, routes) = route(&smf, &tracks).map_err(RenderError::MidiEvents)?;
    if bytes.len() < 12 || usize::from(u16::from_be_bytes([bytes[10], bytes[11]])) != count {
        return Err(fail("declared MIDI track count differs from parsed tracks"));
    }
    let mut events = Vec::new();
    let mut last_tick = 0;
    let mut tempos = BTreeMap::new();
    for (&track, route) in tracks.iter().zip(&routes) {
        let mut tick = 0u64;
        for (index, (event, receiver)) in smf.tracks[track].iter().zip(route).enumerate() {
            tick += u64::from(event.delta.as_int());
            if let TrackEventKind::Meta(MetaMessage::Tempo(tempo)) = event.kind
                && matches!(smf.header.timing, Timing::Metrical(_))
            {
                if tempo.as_int() == 0 {
                    return Err(fail("zero MIDI tempo is not a valid clock"));
                }
                if let Some((previous_track, previous_tempo)) = tempos.insert(tick, (track, tempo))
                    && previous_track != track
                    && previous_tempo != tempo
                {
                    return Err(fail(
                        "conflicting tempos on different tracks have no declared precedence",
                    ));
                }
            }
            events.push((tick, *receiver, (track, index), event.kind));
        }
        last_tick = last_tick.max(tick);
    }
    let (start, end) = match *layout {
        RenderLayout::Full { loop_passes: 1 } => (0, last_tick),
        RenderLayout::Excerpt { start, end } if start < end && end <= last_tick => (start, end),
        _ => {
            return Err(fail(
                "source playback requires one full pass or an excerpt within the source timeline",
            ));
        }
    };
    if end == 0 {
        return Err(fail("source has no timed content"));
    }
    let mut per_tick = match smf.header.timing {
        Timing::Metrical(ppq) if ppq.as_int() > 0 => {
            Rational::new(500_000, i128::from(ppq.as_int()) * 1_000_000)
        }
        Timing::Timecode(fps, ticks) if ticks > 0 => {
            let (num, den) = match fps {
                midly::Fps::Fps24 => (24, 1),
                midly::Fps::Fps25 => (25, 1),
                midly::Fps::Fps29 => (30_000, 1001),
                midly::Fps::Fps30 => (30, 1),
            };
            Rational::new(den, num * i128::from(ticks))
        }
        _ => return Err(fail("source timing division must be positive")),
    };
    // Synthetic boundaries only sample the clock; they never modify the source.
    let boundary = (usize::MAX, 0);
    events.push((
        start,
        None,
        boundary,
        TrackEventKind::Meta(MetaMessage::EndOfTrack),
    ));
    events.push((
        end,
        None,
        boundary,
        TrackEventKind::Meta(MetaMessage::EndOfTrack),
    ));
    events.sort_by_key(|(tick, _, _, _)| *tick);
    let mut seconds = Rational::from_integer(0);
    let mut previous = 0;
    let mut crop = 0;
    let mut content = 0;
    let mut scheduled = Vec::new();
    let mut clock = vec![(0, seconds, per_tick)];
    let mut origins = Vec::new();
    for (tick, receiver, (track, index), kind) in events
        .into_iter()
        .take_while(|(tick, _, _, _)| *tick <= end)
    {
        seconds += per_tick * i128::from(tick - previous);
        previous = tick;
        // Bound memory before allocating audio, including excerpt preroll.
        if seconds > Rational::from_integer(1800)
            || seconds * i128::from(sample_rate) > Rational::from_integer(80_000_000)
        {
            return Err(fail(
                "source preview exceeds 1800 seconds or 80 million preroll frames",
            ));
        }
        let sample = sample_at(seconds, sample_rate);
        if tick == start {
            crop = sample as usize;
        }
        if tick == end {
            content = sample;
        }
        match (kind, receiver) {
            (TrackEventKind::Meta(MetaMessage::Tempo(tempo)), _) => {
                if let Timing::Metrical(ppq) = smf.header.timing {
                    per_tick = Rational::new(
                        i128::from(tempo.as_int()),
                        i128::from(ppq.as_int()) * 1_000_000,
                    );
                    clock.push((tick, seconds, per_tick));
                }
            }
            (TrackEventKind::Midi { channel, message }, Some(receiver)) => {
                scheduled.push((sample, receiver, channel.as_int(), source_message(message)));
                origins.push((tick, track, index));
            }
            _ => {}
        }
    }
    Ok(SourcePlayback {
        scheduled,
        receivers,
        crop,
        content,
        end,
        clock,
        origins,
    })
}

/// Project previews use the same receiver and audible-message checks as source playback.
pub(crate) fn project_preview_blockers(smf: &Smf<'_>) -> Vec<Diagnostic> {
    route(smf, &(0..smf.tracks.len()).collect::<Vec<_>>())
        .err()
        .unwrap_or_default()
}
