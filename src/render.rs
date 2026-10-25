//! Offline preview rendering with Tunemend's SoundFont synthesizer
//! ([`crate::synth`]).
//!
//! The renderer plays the same channel messages as MIDI export. Each message
//! starts at the first PCM sample at or after its exact time, so a sample
//! index maps back to the composition through [`crate::timeline`].

use std::collections::BTreeMap;
use std::sync::Arc;

pub use crate::audio_clips::ClipSources;
use crate::composition::{
    Composition, ControlKind, ControlLane, ControlPoint, Note, Playback, TempoChange, Timebase,
};
use crate::export::VoiceAssignment;
use crate::mix::{Mix, Mixer};
pub use crate::mix::{StemAudio, StemCapture};
use crate::performance::{ChannelMessage, channel_events};
use crate::review::{RenderLayout, WavFormat};
use crate::synth::{SoundFont, Synthesizer};
use crate::timeline::{Rational, content_seconds, seconds_at};
use crate::voice_instruments::{ChipInstrument, render_chip};

/// Chip instruments resolved for one render, by voice id.
pub type ChipInstruments = BTreeMap<String, ChipInstrument>;

/// The decay after the content ends is cut at this length and reported.
pub const MAX_TAIL_SECONDS: u32 = 3;
/// The tail ends once a 10 ms block stays below this peak (about -80 dBFS).
const SILENCE_PEAK: f32 = 1.0e-4;
const PERCUSSION_CHANNEL: u8 = 9;

#[derive(Debug, Clone, PartialEq)]
pub struct RenderedAudio {
    pub sample_rate: u32,
    pub left: Vec<f32>,
    pub right: Vec<f32>,
    /// Samples that hold the musical content; the rest is the decay tail.
    pub content_samples: u64,
    /// The decay was still audible when the tail reached [`MAX_TAIL_SECONDS`].
    pub tail_truncated: bool,
    /// Stems captured beside this mixdown, each as long as it.
    pub stems: Vec<StemAudio>,
    /// With pre-master stems, the master input they sum to.
    pub pre_master_mix: Option<[Vec<f32>; 2]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    SoundFont(String),
    TooManyVoices(usize),
    UnknownVoice(String),
    Synthesizer(String),
    Midi(String),
    /// A chip instrument could not play its voice.
    Instrument(crate::diagnostic::Diagnostic),
    /// An audio clip could not play its source.
    Clip(crate::diagnostic::Diagnostic),
}

/// Per-voice chip reports and per-clip reports of a render.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SourceReports {
    pub instruments: Vec<serde_json::Value>,
    pub clips: Vec<serde_json::Value>,
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Midi(error) => write!(f, "{error}"),
            Self::SoundFont(error) => write!(f, "the sound source could not be loaded: {error}"),
            Self::TooManyVoices(count) => write!(
                f,
                "{count} voices need more than the 15 melodic MIDI channels"
            ),
            Self::UnknownVoice(voice) => write!(f, "voice `{voice}` does not exist"),
            Self::Synthesizer(error) => write!(f, "the synthesizer could not start: {error}"),
            Self::Instrument(diagnostic) | Self::Clip(diagnostic) => {
                write!(f, "{}", diagnostic.message)
            }
        }
    }
}

/// Sample index of an exact time: the first sample at or after it.
pub fn sample_at(seconds: Rational, sample_rate: u32) -> u64 {
    (seconds * i128::from(sample_rate)).ceil().to_integer() as u64
}

/// Renders a checked composition. `voices` limits the render to some voices.
pub fn render(
    timebase: Timebase,
    composition: &Composition,
    layout: &RenderLayout,
    voices: Option<&[String]>,
    sound_font: &[u8],
    sample_rate: u32,
) -> Result<RenderedAudio, RenderError> {
    render_with_instruments(
        timebase,
        composition,
        layout,
        voices,
        sound_font,
        sample_rate,
        &ChipInstruments::new(),
    )
    .map(|(audio, _)| audio)
}

/// [`render`] with voices played by resolved chip instruments instead of the
/// reference synthesizer. Also returns one report per chip-rendered voice.
pub fn render_with_instruments(
    timebase: Timebase,
    composition: &Composition,
    layout: &RenderLayout,
    voices: Option<&[String]>,
    sound_font: &[u8],
    sample_rate: u32,
    instruments: &ChipInstruments,
) -> Result<(RenderedAudio, Vec<serde_json::Value>), RenderError> {
    render_with_sources(
        timebase,
        composition,
        layout,
        voices,
        sound_font,
        sample_rate,
        instruments,
        &ClipSources::new(),
        None,
    )
    .map(|(audio, reports)| (audio, reports.instruments))
}

/// [`render_with_instruments`] that also plays the revision's audio clips
/// from decoded `clips` sources, on their voices' dry signals. With
/// `capture`, every voice renders through the mixer, with its own
/// synthesizer, even when the mixer is empty, and the stems are returned in
/// [`RenderedAudio::stems`].
#[allow(clippy::too_many_arguments)]
pub fn render_with_sources(
    timebase: Timebase,
    composition: &Composition,
    layout: &RenderLayout,
    voices: Option<&[String]>,
    sound_font: &[u8],
    sample_rate: u32,
    instruments: &ChipInstruments,
    clips: &ClipSources,
    capture: Option<&StemCapture>,
) -> Result<(RenderedAudio, SourceReports), RenderError> {
    let effects = composition
        .control_lanes
        .iter()
        .any(|lane| matches!(lane.kind, ControlKind::Reverb | ControlKind::Chorus));
    let mut synthesizer = synthesizer(sound_font, sample_rate, effects)?;

    let selected: Vec<&str> = match voices {
        Some(voices) => {
            for voice in voices {
                if !composition.voices.iter().any(|v| &v.id == voice) {
                    return Err(RenderError::UnknownVoice(voice.clone()));
                }
            }
            composition
                .voices
                .iter()
                .map(|v| v.id.as_str())
                .filter(|id| voices.iter().any(|v| v == id))
                .collect()
        }
        None => composition.voices.iter().map(|v| v.id.as_str()).collect(),
    };
    let channels = (0..16u8).filter(|c| *c != PERCUSSION_CHANNEL);
    if selected.len() > channels.clone().count() {
        return Err(RenderError::TooManyVoices(selected.len()));
    }
    let assignments: Vec<VoiceAssignment> = selected
        .iter()
        .zip(channels)
        .map(|(voice, channel)| VoiceAssignment {
            voice: voice.to_string(),
            track: voice.to_string(),
            channel,
        })
        .collect();

    let (played, played_layout) = match *layout {
        RenderLayout::Excerpt { start, end } => (
            excerpt(composition, start, end),
            RenderLayout::Full { loop_passes: 1 },
        ),
        RenderLayout::Full { .. } => (composition.clone(), *layout),
    };
    let content = sample_at(
        content_seconds(timebase, &played, &played_layout),
        sample_rate,
    );
    let mut scheduled = Vec::new();
    let events = channel_events(&played, &assignments);
    let RenderLayout::Full { loop_passes } = played_layout else {
        unreachable!("excerpts are played in full")
    };
    let loop_start = played.playback.loop_start;
    let intro = seconds_at(
        timebase,
        &played,
        Rational::from_integer(loop_start.unwrap_or(0).into()),
    );
    let loop_length = seconds_at(
        timebase,
        &played,
        Rational::from_integer(played.playback.end.into()),
    ) - intro;
    for pass in 0..loop_passes {
        for event in &events {
            // Later passes replay the loop region; a release exactly at the
            // loop start ends an intro note and is not repeated.
            let in_loop = loop_start.is_some_and(|start| {
                event.at > start
                    || (event.at == start
                        && !matches!(event.message, ChannelMessage::NoteOff { .. }))
            });
            if pass > 0 && !in_loop {
                continue;
            }
            let seconds = seconds_at(timebase, &played, Rational::from_integer(event.at.into()))
                + loop_length * i128::from(pass);
            scheduled.push((sample_at(seconds, sample_rate), seconds, event));
        }
    }
    // Messages at the same sample keep pass order, then note-off, control, note-on.
    scheduled.sort_by_key(|(sample, _, event)| {
        (
            *sample,
            matches!(event.message, ChannelMessage::NoteOn { .. }),
        )
    });

    // An excerpt's mixer has no clips; they are placed from the whole composition.
    if !played.mix.is_empty() || !composition.mix.clips.is_empty() || capture.is_some() {
        let stems = assignments
            .iter()
            .map(|assignment| VoiceEvents {
                voice: assignment.voice.clone(),
                events: scheduled
                    .iter()
                    .filter(|(_, _, event)| event.channel == assignment.channel)
                    .map(|(sample, seconds, event)| {
                        (*sample, *seconds, event.channel, event.message)
                    })
                    .collect(),
            })
            .collect();
        let length = mixed_length(content, sample_rate);
        let mut mixer = Mixer::new(
            timebase,
            &played,
            &played.mix,
            played.playback.end,
            loop_passes,
            sample_rate,
            length,
        );
        if let Some(capture) = capture {
            mixer.capture(capture);
        }
        let placements = crate::audio_clips::layout_placements(
            timebase,
            composition,
            layout,
            sample_rate,
            content,
            voices,
        );
        let (clip_signals, clip_reports) = crate::audio_clips::clip_signals(
            &composition.mix.clips,
            &placements,
            clips,
            sample_rate,
            length,
            |voice| composition.mix.audible(voice),
        )
        .map_err(RenderError::Clip)?;
        let (audio, chip_reports) = render_mixed(
            mixer,
            stems,
            sound_font,
            sample_rate,
            effects,
            content,
            None,
            instruments,
            clip_signals,
        )?;
        return Ok((
            audio,
            SourceReports {
                instruments: chip_reports,
                clips: clip_reports,
            },
        ));
    }

    let mut left = Vec::with_capacity(content as usize);
    let mut right = Vec::with_capacity(content as usize);
    for (sample, _, event) in scheduled {
        render_until(&mut synthesizer, &mut left, &mut right, sample);
        send(&mut synthesizer, event.channel, event.message);
    }
    render_until(&mut synthesizer, &mut left, &mut right, content);

    finish_audio(synthesizer, left, right, content, sample_rate)
        .map(|audio| (audio, SourceReports::default()))
}

/// The scheduled channel messages one voice plays: render sample, exact
/// render time, channel and message.
struct VoiceEvents {
    voice: String,
    events: Vec<(u64, Rational, u8, ChannelMessage)>,
}

/// Samples rendered per voice before mixing: the content and the longest tail.
fn mixed_length(content: u64, sample_rate: u32) -> usize {
    let block = u64::from((sample_rate / 100).max(1));
    let max_tail = u64::from(MAX_TAIL_SECONDS) * u64::from(sample_rate);
    (content + max_tail.div_ceil(block) * block) as usize
}

/// Renders each audible voice with its own synthesizer, or its chip
/// instrument, mixes the results and cuts the tail like [`finish_audio`]:
/// after the first silent 10 ms block. With delay, reverb or echo effects,
/// that block must begin a silent run at least as long as their delay lines,
/// so gaps between echoes are kept. `notes_off_at` releases every note at
/// that time, the content end, as excerpts of preserved MIDI need.
#[allow(clippy::too_many_arguments)]
fn render_mixed(
    mut mixer: Mixer,
    stems: Vec<VoiceEvents>,
    sound_font: &[u8],
    sample_rate: u32,
    effects: bool,
    content: u64,
    notes_off_at: Option<Rational>,
    instruments: &ChipInstruments,
    mut clips: BTreeMap<String, [Vec<f32>; 2]>,
) -> Result<(RenderedAudio, Vec<serde_json::Value>), RenderError> {
    let font = load_font(sound_font)?;
    let length = mixer.length();
    let mut reports = Vec::new();
    // Sidechain key voices render before the voices they key.
    let names: Vec<&str> = stems.iter().map(|s| s.voice.as_str()).collect();
    let order = mixer.voice_order(&names);
    let mut stems: Vec<Option<VoiceEvents>> = stems.into_iter().map(Some).collect();
    for stem in order.into_iter().filter_map(|i| stems[i].take()) {
        if !mixer.audible(&stem.voice) {
            continue;
        }
        let mut dry = if let Some(instrument) = instruments.get(&stem.voice) {
            let events: Vec<(Rational, ChannelMessage)> = stem
                .events
                .iter()
                .map(|&(_, seconds, _, message)| (seconds, message))
                .collect();
            let chip = render_chip(
                &stem.voice,
                instrument,
                &events,
                notes_off_at,
                length,
                sample_rate,
            )
            .map_err(RenderError::Instrument)?;
            reports.push(chip.report);
            chip.signal
        } else {
            let mut synth = synthesizer_for(&font, sample_rate, effects)?;
            let (mut left, mut right) = (Vec::with_capacity(length), Vec::with_capacity(length));
            for (sample, _, channel, message) in stem.events {
                render_until(&mut synth, &mut left, &mut right, sample);
                send(&mut synth, channel, message);
            }
            render_until(&mut synth, &mut left, &mut right, content);
            if notes_off_at.is_some() {
                release_every_note(&mut synth);
            }
            render_until(&mut synth, &mut left, &mut right, length as u64);
            left.truncate(length);
            right.truncate(length);
            [left, right]
        };
        // Clips sum into the voice's dry signal, before its effect chain.
        if let Some(clip) = clips.remove(&stem.voice) {
            for (side, clip_side) in dry.iter_mut().zip(clip) {
                side.resize(length, 0.0);
                for (out, value) in side.iter_mut().zip(clip_side) {
                    *out = (f64::from(*out) + f64::from(value)) as f32;
                }
            }
        }
        mixer.add_voice(&stem.voice, dry);
    }
    let hold = mixer.silence_hold();
    let output = mixer.finish();
    let [mut left, mut right] = output.mixdown;
    let block = (sample_rate / 100).max(1) as usize;
    let silent_blocks = hold.div_ceil(block).max(1);
    let mut kept = content as usize;
    let mut tail_truncated = true;
    let (mut run, mut run_end) = (0, kept);
    while kept < length {
        let end = (kept + block).min(length);
        let peak = left[kept..end]
            .iter()
            .chain(&right[kept..end])
            .fold(0.0f32, |peak, s| peak.max(s.abs()));
        kept = end;
        if peak < SILENCE_PEAK {
            if run == 0 {
                run_end = end;
            }
            run += 1;
            if run >= silent_blocks {
                kept = run_end;
                tail_truncated = false;
                break;
            }
        } else {
            run = 0;
        }
    }
    left.truncate(kept);
    right.truncate(kept);
    let mut stems = output.stems;
    for stem in &mut stems {
        stem.left.truncate(kept);
        stem.right.truncate(kept);
    }
    let pre_master_mix = output.pre_master_mix.map(|[mut l, mut r]| {
        l.truncate(kept);
        r.truncate(kept);
        [l, r]
    });
    Ok((
        RenderedAudio {
            sample_rate,
            left,
            right,
            content_samples: content,
            tail_truncated,
            stems,
            pre_master_mix,
        },
        reports,
    ))
}

fn synthesizer(bytes: &[u8], sample_rate: u32, effects: bool) -> Result<Synthesizer, RenderError> {
    synthesizer_for(&load_font(bytes)?, sample_rate, effects)
}

fn load_font(bytes: &[u8]) -> Result<Arc<SoundFont>, RenderError> {
    Ok(Arc::new(
        SoundFont::parse(bytes).map_err(|e| RenderError::SoundFont(e.to_string()))?,
    ))
}

fn synthesizer_for(
    font: &Arc<SoundFont>,
    sample_rate: u32,
    effects: bool,
) -> Result<Synthesizer, RenderError> {
    // Reverb and chorus sends start at 0 on every channel.
    Synthesizer::new(font.clone(), sample_rate, effects)
        .map_err(|e| RenderError::Synthesizer(e.to_string()))
}

/// Keys off every note at an excerpt end: sustain and sostenuto pedals
/// lift, then All Notes Off.
fn release_every_note(synthesizer: &mut Synthesizer) {
    for channel in 0..16 {
        synthesizer.control_change(channel, 64, 0);
        synthesizer.control_change(channel, 66, 0);
        synthesizer.control_change(channel, 123, 0);
    }
}

fn finish_audio(
    mut synthesizer: Synthesizer,
    mut left: Vec<f32>,
    mut right: Vec<f32>,
    content: u64,
    sample_rate: u32,
) -> Result<RenderedAudio, RenderError> {
    let block = (sample_rate / 100).max(1) as u64;
    let max_tail = u64::from(MAX_TAIL_SECONDS) * u64::from(sample_rate);
    let mut tail_truncated = true;
    while (left.len() as u64) < content + max_tail {
        let start = left.len();
        render_until(
            &mut synthesizer,
            &mut left,
            &mut right,
            start as u64 + block,
        );
        let peak = left[start..]
            .iter()
            .chain(&right[start..])
            .fold(0.0f32, |peak, s| peak.max(s.abs()));
        if peak < SILENCE_PEAK {
            tail_truncated = false;
            break;
        }
    }
    Ok(RenderedAudio {
        sample_rate,
        left,
        right,
        content_samples: content,
        tail_truncated,
        stems: Vec::new(),
        pre_master_mix: None,
    })
}

fn render_until(
    synthesizer: &mut Synthesizer,
    left: &mut Vec<f32>,
    right: &mut Vec<f32>,
    sample: u64,
) {
    let start = left.len();
    let missing = sample.saturating_sub(start as u64) as usize;
    if missing > 0 {
        left.resize(start + missing, 0.0);
        right.resize(start + missing, 0.0);
        synthesizer.render(&mut left[start..], &mut right[start..]);
    }
}

fn send(synthesizer: &mut Synthesizer, channel: u8, message: ChannelMessage) {
    match message {
        ChannelMessage::NoteOn { key, velocity } => synthesizer.note_on(channel, key, velocity),
        ChannelMessage::NoteOff { key, .. } => synthesizer.note_off(channel, key),
        ChannelMessage::Controller { controller, value } => {
            synthesizer.control_change(channel, controller, value)
        }
        ChannelMessage::Program(program) => synthesizer.program_change(channel, program),
        ChannelMessage::PitchBend(bend) => {
            synthesizer.pitch_bend(channel, (i32::from(bend) + 8192).clamp(0, 16383) as u16)
        }
        ChannelMessage::ChannelPressure(value) => synthesizer.channel_pressure(channel, value),
    }
}

/// The part `[start, end)` as its own composition starting at 0. Controller
/// values and tempo in effect at `start` apply from 0, and notes sounding at
/// `start` are struck again there.
pub fn excerpt(composition: &Composition, start: u64, end: u64) -> Composition {
    let shift = |at: u64| at.saturating_sub(start);
    let notes = composition
        .notes
        .iter()
        .filter(|n| n.start < end && n.start + n.duration > start)
        .map(|n| {
            let (from, to) = (n.start.max(start), (n.start + n.duration).min(end));
            Note {
                start: from - start,
                duration: to - from,
                ..n.clone()
            }
        })
        .collect();
    let control_lanes = composition
        .control_lanes
        .iter()
        .filter_map(|lane| {
            let mut points: Vec<ControlPoint> = Vec::new();
            if let Some(held) = lane.points.iter().take_while(|p| p.at <= start).last() {
                points.push(ControlPoint {
                    at: 0,
                    value: held.value,
                });
            }
            points.extend(
                lane.points
                    .iter()
                    .filter(|p| p.at > start && p.at < end)
                    .map(|p| ControlPoint {
                        at: shift(p.at),
                        ..*p
                    }),
            );
            (!points.is_empty()).then(|| ControlLane {
                points,
                ..lane.clone()
            })
        })
        .collect();
    let mut tempo_map: Vec<TempoChange> = Vec::new();
    if let Some(held) = composition
        .tempo_map
        .iter()
        .take_while(|t| t.tick <= start)
        .last()
    {
        tempo_map.push(TempoChange { tick: 0, ..*held });
    }
    tempo_map.extend(
        composition
            .tempo_map
            .iter()
            .filter(|t| t.tick > start && t.tick < end)
            .map(|t| TempoChange {
                tick: shift(t.tick),
                ..*t
            }),
    );
    Composition {
        tempo_map,
        time_signatures: vec![],
        key_signatures: vec![],
        markers: vec![],
        voices: composition.voices.clone(),
        notes,
        control_lanes,
        mix: crate::mix::excerpt_mix(&composition.mix, start, end),
        sections: vec![],
        playback: Playback {
            end: end - start,
            loop_start: None,
        },
    }
}

/// 16-bit stereo PCM WAV bytes.
pub fn wav_bytes(audio: &RenderedAudio) -> Vec<u8> {
    let frames = audio.left.len() as u32;
    let data_length = frames * 4;
    let mut out = Vec::with_capacity(44 + data_length as usize);
    out.extend(b"RIFF");
    out.extend((36 + data_length).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(2u16.to_le_bytes());
    out.extend(audio.sample_rate.to_le_bytes());
    out.extend((audio.sample_rate * 4).to_le_bytes());
    out.extend(4u16.to_le_bytes());
    out.extend(16u16.to_le_bytes());
    out.extend(b"data");
    out.extend(data_length.to_le_bytes());
    for (l, r) in audio.left.iter().zip(&audio.right) {
        for sample in [l, r] {
            out.extend(((sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16).to_le_bytes());
        }
    }
    out
}

/// Stereo WAV bytes of `left` and `right` in `format`. 16-bit PCM is
/// [`wav_bytes`] exactly. 24-bit PCM stores `round(clamp(x, -1, 1) ×
/// 8388607)` computed in f64. 32-bit float (format tag 3, with a `fact`
/// chunk) stores every f32 sample unchanged, without clamping.
pub fn wav_bytes_in(sample_rate: u32, left: &[f32], right: &[f32], format: WavFormat) -> Vec<u8> {
    let frames = left.len().min(right.len());
    let (tag, bytes_per_sample): (u16, u32) = match format {
        WavFormat::Pcm16 => {
            return wav_bytes(&RenderedAudio {
                sample_rate,
                left: left.to_vec(),
                right: right.to_vec(),
                content_samples: 0,
                tail_truncated: false,
                stems: Vec::new(),
                pre_master_mix: None,
            });
        }
        WavFormat::Pcm24 => (1, 3),
        WavFormat::Float32 => (3, 4),
    };
    let block = 2 * bytes_per_sample;
    let data_length = frames as u32 * block;
    let float = format == WavFormat::Float32;
    let fmt_length: u32 = if float { 18 } else { 16 };
    let fact_length: u32 = if float { 12 } else { 0 };
    let mut out = Vec::with_capacity(28 + (fmt_length + fact_length + data_length) as usize);
    out.extend(b"RIFF");
    out.extend((4 + 8 + fmt_length + fact_length + 8 + data_length).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(fmt_length.to_le_bytes());
    out.extend(tag.to_le_bytes());
    out.extend(2u16.to_le_bytes());
    out.extend(sample_rate.to_le_bytes());
    out.extend((sample_rate * block).to_le_bytes());
    out.extend((block as u16).to_le_bytes());
    out.extend(((bytes_per_sample * 8) as u16).to_le_bytes());
    if float {
        out.extend(0u16.to_le_bytes());
        out.extend(b"fact");
        out.extend(4u32.to_le_bytes());
        out.extend((frames as u32).to_le_bytes());
    }
    out.extend(b"data");
    out.extend(data_length.to_le_bytes());
    for (l, r) in left.iter().zip(right).take(frames) {
        for &sample in [l, r] {
            if float {
                out.extend(sample.to_le_bytes());
            } else {
                let code = (f64::from(sample.clamp(-1.0, 1.0)) * 8_388_607.0).round() as i32;
                out.extend(&code.to_le_bytes()[..3]);
            }
        }
    }
    out
}

/// Render the preserved event stream, including same-tick order and original
/// channels. Excerpts run the preceding events before cropping the PCM.
pub fn render_midi(
    bytes: &[u8],
    layout: &RenderLayout,
    voices: Option<&[String]>,
    sound_font: &[u8],
    sample_rate: u32,
) -> Result<RenderedAudio, RenderError> {
    render_midi_mixed(
        bytes,
        layout,
        voices,
        sound_font,
        sample_rate,
        &Mix::default(),
    )
}

/// [`render_midi`] followed by a revision's mixer. Without mixer data the
/// result is identical to [`render_midi`]; with it, each voice renders with
/// its own synthesizer, receiving its notes and its channel's controls.
pub fn render_midi_mixed(
    bytes: &[u8],
    layout: &RenderLayout,
    voices: Option<&[String]>,
    sound_font: &[u8],
    sample_rate: u32,
    mix: &Mix,
) -> Result<RenderedAudio, RenderError> {
    render_midi_with_instruments(
        bytes,
        layout,
        voices,
        sound_font,
        sample_rate,
        mix,
        &ChipInstruments::new(),
    )
    .map(|(audio, _)| audio)
}

/// [`render_midi_mixed`] with voices played by resolved chip instruments.
/// Chip state, like the synthesizer's, runs from the file's start, so an
/// excerpt hears each instrument as it is at the excerpt start.
pub fn render_midi_with_instruments(
    bytes: &[u8],
    layout: &RenderLayout,
    voices: Option<&[String]>,
    sound_font: &[u8],
    sample_rate: u32,
    mix: &Mix,
    instruments: &ChipInstruments,
) -> Result<(RenderedAudio, Vec<serde_json::Value>), RenderError> {
    render_midi_with_sources(
        bytes,
        layout,
        voices,
        sound_font,
        sample_rate,
        mix,
        instruments,
        &ClipSources::new(),
        None,
    )
    .map(|(audio, reports)| (audio, reports.instruments))
}

/// [`render_midi_with_instruments`] that also plays the mixer's audio clips.
/// Clips are placed on the file's full timeline like its events, so an
/// excerpt is the full render's clip audio cropped. `capture` works as in
/// [`render_with_sources`]; excerpt stems are cropped like the mixdown.
#[allow(clippy::too_many_arguments)]
pub fn render_midi_with_sources(
    bytes: &[u8],
    layout: &RenderLayout,
    voices: Option<&[String]>,
    sound_font: &[u8],
    sample_rate: u32,
    mix: &Mix,
    instruments: &ChipInstruments,
    clips: &ClipSources,
    capture: Option<&StemCapture>,
) -> Result<(RenderedAudio, SourceReports), RenderError> {
    use midly::{MidiMessage, Smf, TrackEventKind};
    let diagnostics = crate::midi::preview_blockers(bytes);
    if !diagnostics.is_empty() {
        return Err(RenderError::Midi(format!(
            "{} events cannot be interpreted by the reference synthesizer; use analyze_midi for locations",
            diagnostics.len()
        )));
    }
    let imported =
        crate::midi::read_smf_preserving(bytes).map_err(|d| RenderError::Midi(format!("{d:?}")))?;
    let composition = &imported.composition;
    let invalid_layout = match *layout {
        RenderLayout::Full { loop_passes } => {
            loop_passes == 0 || (loop_passes > 1 && composition.playback.loop_start.is_none())
        }
        RenderLayout::Excerpt { start, end } => start >= end || end > composition.playback.end,
    };
    if invalid_layout {
        return Err(RenderError::Midi("invalid source render layout".into()));
    }
    if let Some(voices) = voices {
        for voice in voices {
            if !composition.voices.iter().any(|v| &v.id == voice) {
                return Err(RenderError::UnknownVoice(voice.clone()));
            }
        }
    }
    let smf = Smf::parse(bytes).map_err(|e| RenderError::Midi(e.to_string()))?;
    let mut events = Vec::new();
    for (track, messages) in smf.tracks.iter().enumerate() {
        let mut tick = 0;
        for event in messages {
            tick += u64::from(event.delta.as_int());
            let TrackEventKind::Midi { channel, message } = event.kind else {
                continue;
            };
            let voice = format!("track{track}-ch{}", channel.as_int() + 1);
            if voices.is_some_and(|selected| !selected.contains(&voice)) {
                continue;
            }
            let message = match message {
                MidiMessage::NoteOff { key, vel } => ChannelMessage::NoteOff {
                    key: key.as_int(),
                    velocity: vel.as_int(),
                },
                MidiMessage::NoteOn { key, vel } if vel.as_int() == 0 => ChannelMessage::NoteOff {
                    key: key.as_int(),
                    velocity: 64,
                },
                MidiMessage::NoteOn { key, vel } => ChannelMessage::NoteOn {
                    key: key.as_int(),
                    velocity: vel.as_int(),
                },
                MidiMessage::Controller { controller, value } => ChannelMessage::Controller {
                    controller: controller.as_int(),
                    value: value.as_int(),
                },
                MidiMessage::ProgramChange { program } => ChannelMessage::Program(program.as_int()),
                MidiMessage::PitchBend { bend } => ChannelMessage::PitchBend(bend.as_int()),
                MidiMessage::ChannelAftertouch { vel } => {
                    ChannelMessage::ChannelPressure(vel.as_int())
                }
                _ => unreachable!("preview blockers reject aftertouch"),
            };
            events.push((
                crate::performance::ChannelEvent {
                    at: tick,
                    channel: channel.as_int(),
                    message,
                },
                voice,
            ));
        }
    }
    // Stable sort: preserve every track's original order at an equal tick.
    events.sort_by_key(|(event, _)| event.at);
    let (start, end, passes) = match *layout {
        RenderLayout::Excerpt { start, end } => (start, end, 1),
        RenderLayout::Full { loop_passes } => (0, composition.playback.end, loop_passes),
    };
    let seconds = |tick| seconds_at(imported.timebase, composition, Rational::from_integer(tick));
    let loop_length =
        seconds(end.into()) - seconds(composition.playback.loop_start.unwrap_or(0).into());
    let content = sample_at(
        seconds(end.into()) + loop_length * i128::from(passes - 1),
        sample_rate,
    );
    let mut scheduled = Vec::new();
    for pass in 0..passes {
        for (event, voice) in &events {
            if event.at > end {
                continue;
            }
            if pass > 0
                && !composition.playback.loop_start.is_some_and(|start| {
                    event.at > start
                        || (event.at == start
                            && !matches!(event.message, ChannelMessage::NoteOff { .. }))
                })
            {
                continue;
            }
            let at = seconds(event.at.into()) + loop_length * i128::from(pass);
            scheduled.push((sample_at(at, sample_rate), at, event, voice));
        }
    }
    scheduled.sort_by_key(|(sample, _, _, _)| *sample);
    let effects = composition
        .control_lanes
        .iter()
        .any(|lane| matches!(lane.kind, ControlKind::Reverb | ControlKind::Chorus));
    let crop = sample_at(seconds(start.into()), sample_rate) as usize;
    if !mix.is_empty() || capture.is_some() {
        let stems = composition
            .voices
            .iter()
            .filter(|v| voices.is_none_or(|selected| selected.contains(&v.id)))
            .map(|v| {
                let channels: Vec<u8> = scheduled
                    .iter()
                    .filter(|(_, _, _, voice)| **voice == v.id)
                    .map(|(_, _, event, _)| event.channel)
                    .collect();
                VoiceEvents {
                    voice: v.id.clone(),
                    events: scheduled
                        .iter()
                        .filter(|(_, _, event, voice)| {
                            **voice == v.id
                                || (channels.contains(&event.channel)
                                    && !matches!(
                                        event.message,
                                        ChannelMessage::NoteOn { .. }
                                            | ChannelMessage::NoteOff { .. }
                                    ))
                        })
                        .map(|(sample, at, event, _)| (*sample, *at, event.channel, event.message))
                        .collect(),
                }
            })
            .collect();
        let length = mixed_length(content, sample_rate);
        let mut mixer = Mixer::new(
            imported.timebase,
            composition,
            mix,
            end,
            passes,
            sample_rate,
            length,
        );
        if let Some(capture) = capture {
            mixer.capture(capture);
        }
        let placements = crate::audio_clips::full_placements(
            imported.timebase,
            composition,
            &mix.clips,
            passes,
            sample_rate,
            content,
            voices,
        )
        .into_iter()
        .filter(|p| p.limit > crop as i64)
        .collect::<Vec<_>>();
        let (clip_signals, clip_reports) = crate::audio_clips::clip_signals(
            &mix.clips,
            &placements,
            clips,
            sample_rate,
            length,
            |voice| mix.audible(voice),
        )
        .map_err(RenderError::Clip)?;
        let (mut audio, reports) = render_mixed(
            mixer,
            stems,
            sound_font,
            sample_rate,
            effects,
            content,
            matches!(layout, RenderLayout::Excerpt { .. }).then(|| seconds(end.into())),
            instruments,
            clip_signals,
        )?;
        audio.left.drain(..crop);
        audio.right.drain(..crop);
        audio.content_samples -= crop as u64;
        for side in audio
            .stems
            .iter_mut()
            .flat_map(|s| [&mut s.left, &mut s.right])
            .chain(audio.pre_master_mix.iter_mut().flatten())
        {
            side.drain(..crop.min(side.len()));
        }
        return Ok((
            audio,
            SourceReports {
                instruments: reports,
                clips: clip_reports,
            },
        ));
    }
    let mut synth = synthesizer(sound_font, sample_rate, effects)?;
    let (mut left, mut right) = (Vec::new(), Vec::new());
    for (sample, _, event, _) in scheduled {
        render_until(&mut synth, &mut left, &mut right, sample);
        send(&mut synth, event.channel, event.message);
    }
    render_until(&mut synth, &mut left, &mut right, content);
    if matches!(layout, RenderLayout::Excerpt { .. }) {
        release_every_note(&mut synth);
    }
    let mut audio = finish_audio(synth, left, right, content, sample_rate)?;
    audio.left.drain(..crop);
    audio.right.drain(..crop);
    audio.content_samples -= crop as u64;
    Ok((audio, SourceReports::default()))
}
