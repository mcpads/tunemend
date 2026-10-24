//! Resolving audio clips against retained sound sources.
use super::*;
use crate::audio_clips::{AudioClip, ClipSource, ClipSources, check_against_source};
use crate::mix::Mix;

impl Service {
    /// Decoded sources of the clips a render plays (on `voices`, or every
    /// voice), loaded with identity checks. `base` locates the mixer, such
    /// as `/mix`.
    pub(super) fn clip_sources(
        &mut self,
        mix: &Mix,
        voices: Option<&[String]>,
        base: &str,
    ) -> Result<ClipSources, ToolResponse> {
        let mut sources = ClipSources::new();
        for (index, clip) in mix.clips.iter().enumerate() {
            if voices.is_some_and(|v| !v.contains(&clip.voice)) {
                continue;
            }
            self.clip_source(index, clip, base, &mut sources)?;
        }
        Ok(sources)
    }

    fn clip_source<'a>(
        &mut self,
        index: usize,
        clip: &AudioClip,
        base: &str,
        sources: &'a mut ClipSources,
    ) -> Result<&'a ClipSource, ToolResponse> {
        let at = format!("{base}/clips/{index}");
        if !sources.contains_key(&clip.source) {
            let bytes = self.sound_source_bytes(&clip.source).map_err(|mut r| {
                for d in &mut r.diagnostics {
                    d.location = format!("{at}/source");
                    d.message = format!(
                        "clip `{}`: unknown sound source {}; retain the WAV with load_instrument_source first",
                        clip.id, clip.source
                    );
                }
                r
            })?;
            let decoded = ClipSource::decode(&bytes).map_err(|message| {
                ToolResponse::refused(
                    Code::InvalidAudio,
                    &format!("{at}/source"),
                    format!("clip `{}`: {message}", clip.id),
                )
            })?;
            sources.insert(clip.source.clone(), decoded);
        }
        let source = &sources[&clip.source];
        check_against_source(clip, source).map_err(|message| {
            ToolResponse::refused(
                Code::InvalidAudio,
                &at,
                format!("clip `{}`: {message}", clip.id),
            )
        })?;
        Ok(source)
    }

    /// Resolves the clips an edit added or changed, so unknown or unusable
    /// sources refuse the edit. Returns what each changed clip will play.
    pub(super) fn check_changed_clips(
        &mut self,
        timebase: Timebase,
        base: &Composition,
        result: &Composition,
    ) -> Result<Vec<Value>, ToolResponse> {
        let changed = crate::audio_clips::changed_clips(&base.mix.clips, &result.mix.clips);
        let mut sources = ClipSources::new();
        let mut facts = Vec::new();
        for (index, clip) in result.mix.clips.iter().enumerate() {
            if !changed.contains(&clip.id.as_str()) {
                continue;
            }
            let source = self
                .clip_source(index, clip, "/result/mix", &mut sources)
                .map_err(|r| {
                    if r.status == Status::Blocked {
                        ToolResponse::blocked(
                            "the edit was not applied; nothing changed",
                            r.diagnostics,
                        )
                    } else {
                        r
                    }
                })?;
            let frames = clip.source_end.unwrap_or(source.frames()) - clip.source_start;
            let seconds = |at: Rational| seconds_at(timebase, result, at);
            let start = seconds(Rational::from_integer(clip.start.into()));
            let audio = Rational::new(i128::from(frames), i128::from(source.rate));
            let span = seconds(Rational::from_integer(clip.end().unwrap_or(0).into())) - start;
            let fits = crate::timeline::position_at_seconds(timebase, result, start + audio)
                .ceil()
                .to_integer()
                - i128::from(clip.start);
            facts.push(json!({"clip": clip.id, "voice": clip.voice, "source": clip.source,
                "source_rate": source.rate, "source_channels": source.channels.len(),
                "source_frames": source.frames(), "played_frames": frames,
                "audio_seconds": rational_seconds(audio), "span_seconds": rational_seconds(span),
                "duration_for_audio": fits,
                "source_exhausted": i128::from(clip.duration) > fits,
                "note": "the span cuts longer audio; shorter audio is followed by silence (source_exhausted when that silence is at least one tick; a span of duration_for_audio leaves less, from rounding up to whole ticks). duration_for_audio is the span that plays the whole range at the current tempo map; renders resample whenever their rate differs from source_rate"}));
        }
        Ok(facts)
    }
}
