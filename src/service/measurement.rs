//! Read-only level, loudness and spectrum measurement of stored renders and
//! workspace WAV files, and comparison of two of them.

use super::*;
use crate::measure::{
    self, Audio, Bands, MAX_OFFSET_SECONDS, MAX_SAMPLE_VALUES, MAX_WAV_BYTES, MeasureOptions,
    pitch::{self, PitchConfig, PitchSettings},
    wav::{self, WavFormat},
};
use crate::wav::WavError;

/// A stored render (`project` and `render`) or a WAV file (`path`), exactly one.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AudioInput {
    /// Project holding the render.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Render ID returned by render_preview, such as `render-3`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render: Option<String>,
    /// WAV file relative to the workspace: integer PCM 8/16/24/32-bit or
    /// float 32/64-bit, plain or extensible, 1–8 channels, 8–384 kHz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Refuse when the file's SHA-256 differs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_sha256: Option<String>,
}

/// Part of the input to measure, in seconds from its start.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AudioRange {
    #[serde(default)]
    pub start_seconds: f64,
    /// Defaults to the end of the input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_seconds: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MeasureAudioInput {
    pub audio: AudioInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<AudioRange>,
    /// Also report levels and loudness of each channel alone.
    #[serde(default)]
    pub per_channel: bool,
    /// Band layout of the spectrum; default third_octave.
    #[serde(default)]
    pub spectrum: Bands,
    /// Level below which leading and trailing frames count as silence; default -60.
    /// Pitch windows below it are reported as silent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub silence_threshold_dbfs: Option<f64>,
    /// Also track the fundamental frequency over time (YIN) of the channel
    /// average: f0, nearest note and cents per window, note segments and a
    /// summary. For monophonic material; measure a voice's solo render
    /// rather than a mix, or a chip channel rendered alone (render_psg
    /// `channels`). Runs shorter than `min_note_windows` are not listed as
    /// notes; notes carry `low_confidence` (few_windows, dc_dominated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pitch: Option<PitchSettings>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompareAudioInput {
    /// The earlier or accepted version.
    pub reference: AudioInput,
    pub candidate: AudioInput,
    /// Applied to both inputs, unless `reference_range` or `candidate_range`
    /// gives that input its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<AudioRange>,
    /// The reference's own range, in seconds from its start; replaces `range`
    /// for the reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_range: Option<AudioRange>,
    /// The candidate's own range, in seconds from its start; replaces `range`
    /// for the candidate. With different ranges of one render, loop pass N
    /// (reference) can be compared with pass N+1 (candidate): times in the
    /// result are reference times.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_range: Option<AudioRange>,
    /// Search candidate offsets within ± this many seconds (at most 2) before
    /// taking sample differences; default 0 compares sample for sample.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_offset_seconds: Option<f64>,
    /// Band layout for per-band level differences; default third_octave.
    #[serde(default)]
    pub spectrum: Bands,
    /// Also compare the fundamental frequency (YIN) window by window at the
    /// found alignment: difference in cents, windows whose nearest notes
    /// differ, and the largest differences. `windows` is ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pitch: Option<PitchSettings>,
}

struct Loaded {
    audio: Audio,
    format: WavFormat,
    description: Value,
    start_seconds: f64,
}

const CONVENTIONS: &str = "dBFS relative to full scale 1.0; rms_dbfs and band levels are 10·log10(mean square), so a full-scale sine reads -3.01; loudness per ITU-R BS.1770-4 with EBU Tech 3341/3342 windows; true peak by 4x oversampling; null means silence (minus infinity) or too short to measure";

fn audio_refused(location: &str, error: &WavError) -> ToolResponse {
    let mut response = ToolResponse::refused(
        Code::InvalidAudio,
        location,
        format!("the WAV cannot be measured: {error}"),
    );
    response.result = json!({"byte_offset": error.offset, "field": error.field});
    response
}

/// Resolves pitch settings for an input's rate and checks the window count.
fn pitch_config(
    settings: &PitchSettings,
    loaded: &Loaded,
    silence_threshold_dbfs: f64,
) -> Result<PitchConfig, ToolResponse> {
    let config = settings
        .resolve(loaded.format.sample_rate, silence_threshold_dbfs)
        .map_err(|(field, message)| {
            ToolResponse::refused(
                Code::ValueOutOfRange,
                &format!("/pitch/{field}"),
                format!("{field} {message}"),
            )
        })?;
    let windows = pitch::window_starts(loaded.audio.frames(), &config).len();
    if windows > pitch::MAX_WINDOWS {
        return Err(ToolResponse::refused(
            Code::ValueOutOfRange,
            "/pitch/hop_seconds",
            format!(
                "{windows} pitch windows exceed the limit of {}; use a larger hop_seconds or a shorter range",
                pitch::MAX_WINDOWS
            ),
        ));
    }
    Ok(config)
}

impl Service {
    fn audio_bytes(
        &mut self,
        input: &AudioInput,
        pointer: &str,
    ) -> Result<(Vec<u8>, Value, String), ToolResponse> {
        let (bytes, description, location) = match input {
            AudioInput {
                project: Some(project),
                render: Some(render),
                path: None,
                ..
            } => {
                self.project(project)?;
                let store = self.store_ref()?;
                let record: RenderRecord = store
                    .reader()
                    .document(project, "render", render)?
                    .ok_or_else(|| {
                        ToolResponse::refused(
                            Code::UnknownReference,
                            &format!("{pointer}/render"),
                            format!("render `{render}` does not exist in `{project}`"),
                        )
                    })?;
                if !record.audio_present(store) {
                    return Err(absent_render_audio(
                        project,
                        render,
                        &format!("{pointer}/render"),
                    ));
                }
                let path = store.object_path(&record.object);
                let bytes = std::fs::read(&path).map_err(|e| {
                    ToolResponse::error(format!("the stored render file is unavailable: {e}"))
                })?;
                if sha256_hex(&bytes) != record.object {
                    return Err(ToolResponse::error(format!(
                        "the stored file of `{render}` does not match its hash"
                    )));
                }
                let description = json!({"kind": "render", "project": project, "render": render,
                    "revision": record.render.revision, "layout": record.render.layout,
                    "voices": record.render.voices, "path": path.display().to_string()});
                (bytes, description, format!("{pointer}/render"))
            }
            AudioInput {
                project: None,
                render: None,
                path: Some(path),
                ..
            } => {
                let location = format!("{pointer}/path");
                let full = self.workspace.join(path);
                let size = std::fs::metadata(&full)
                    .map_err(|e| ToolResponse::error(format!("cannot read {path}: {e}")))?
                    .len();
                if size > MAX_WAV_BYTES {
                    return Err(ToolResponse::refused(
                        Code::ValueOutOfRange,
                        &location,
                        format!("{path} has {size} bytes; at most {MAX_WAV_BYTES} are measured"),
                    ));
                }
                let bytes = std::fs::read(&full)
                    .map_err(|e| ToolResponse::error(format!("cannot read {path}: {e}")))?;
                (bytes, json!({"kind": "file", "path": path}), location)
            }
            _ => {
                return Err(ToolResponse::refused(
                    Code::InvalidRange,
                    pointer,
                    "supply either project and render, or path",
                ));
            }
        };
        let sha256 = sha256_hex(&bytes);
        if input
            .expected_sha256
            .as_ref()
            .is_some_and(|expected| expected != &sha256)
        {
            return Err(ToolResponse::refused(
                Code::InvalidRange,
                &format!("{pointer}/expected_sha256"),
                "the audio changed since it was inspected",
            ));
        }
        let mut description = description;
        description["sha256"] = json!(sha256);
        Ok((bytes, description, location))
    }

    fn load_audio(
        &mut self,
        input: &AudioInput,
        (range, range_pointer): (Option<AudioRange>, &str),
        pointer: &str,
    ) -> Result<Loaded, ToolResponse> {
        let (bytes, description, location) = self.audio_bytes(input, pointer)?;
        let format = wav::read_header(&bytes).map_err(|e| audio_refused(&location, &e))?;
        let rate = f64::from(format.sample_rate);
        let duration = format.duration_seconds();
        let range = range.unwrap_or(AudioRange {
            start_seconds: 0.0,
            end_seconds: None,
        });
        let end = range.end_seconds.unwrap_or(duration);
        let out_of_range = |field: &str, message: String| {
            ToolResponse::refused(
                Code::ValueOutOfRange,
                &format!("{range_pointer}/{field}"),
                message,
            )
        };
        if !range.start_seconds.is_finite() || range.start_seconds < 0.0 {
            return Err(out_of_range(
                "start_seconds",
                "the start must be a finite non-negative time".into(),
            ));
        }
        if !end.is_finite() || end <= range.start_seconds || end > duration + 0.5 / rate {
            return Err(out_of_range(
                "end_seconds",
                format!("the end must be after the start and within the input's {duration:.6} s"),
            ));
        }
        let start_frame = ((range.start_seconds * rate).round() as u64).min(format.frames);
        let end_frame = ((end * rate).round() as u64).min(format.frames);
        let frames = end_frame.saturating_sub(start_frame);
        if frames == 0 {
            return Err(out_of_range(
                "end_seconds",
                "the range holds no whole frame".into(),
            ));
        }
        let values = frames * u64::from(format.channels);
        if values > MAX_SAMPLE_VALUES {
            return Err(ToolResponse::refused(
                Code::ValueOutOfRange,
                &if range.end_seconds.is_some() {
                    format!("{range_pointer}/end_seconds")
                } else {
                    location.clone()
                },
                format!(
                    "{values} sample values exceed the limit of {MAX_SAMPLE_VALUES}; measure a shorter range"
                ),
            ));
        }
        let channels = wav::decode(&bytes, &format, start_frame, frames)
            .map_err(|e| audio_refused(&location, &e))?;
        let mut description = description;
        description["format"] = serde_json::to_value(&format).expect("serializes");
        description["duration_seconds"] = json!(duration);
        description["range"] = json!({"start_seconds": start_frame as f64 / rate,
            "end_seconds": end_frame as f64 / rate, "start_frame": start_frame, "frames": frames});
        Ok(Loaded {
            audio: Audio {
                sample_rate: format.sample_rate,
                channels,
                clip_level: format.encoding.clip_level(),
            },
            format,
            description,
            start_seconds: start_frame as f64 / rate,
        })
    }

    pub(super) fn measure_audio(&mut self, input: MeasureAudioInput) -> Handled {
        let threshold = input
            .silence_threshold_dbfs
            .unwrap_or(measure::DEFAULT_SILENCE_DBFS);
        if !(-150.0..=0.0).contains(&threshold) {
            return done(ToolResponse::refused(
                Code::ValueOutOfRange,
                "/silence_threshold_dbfs",
                "the silence threshold must be -150 to 0 dBFS",
            ));
        }
        let loaded = self.load_audio(&input.audio, (input.range, "/range"), "/audio")?;
        let measurement = measure::measure(
            &loaded.audio,
            &MeasureOptions {
                bands: input.spectrum,
                per_channel: input.per_channel,
                silence_threshold_dbfs: threshold,
            },
        );
        let pitch_track = match &input.pitch {
            Some(settings) => {
                let config = pitch_config(settings, &loaded, threshold)?;
                let windows = pitch::window_starts(loaded.audio.frames(), &config).len();
                if config.list_windows && windows > pitch::MAX_LISTED_WINDOWS {
                    return done(ToolResponse::refused(
                        Code::ValueOutOfRange,
                        "/pitch/windows",
                        format!(
                            "{windows} pitch windows exceed the {} that can be listed; use a larger hop_seconds, a shorter range, or windows: false for the summary and note segments only",
                            pitch::MAX_LISTED_WINDOWS
                        ),
                    ));
                }
                Some(pitch::track(
                    &loaded.audio.channels,
                    loaded.format.sample_rate,
                    &config,
                    loaded.start_seconds,
                ))
            }
            None => None,
        };
        let level = |v: Option<f64>, unit: &str| {
            v.map_or_else(|| "silent".to_owned(), |v| format!("{v:.1} {unit}"))
        };
        let mut summary = format!(
            "{}: integrated {}, true peak {}, LRA {}",
            loaded.description["render"]
                .as_str()
                .or(loaded.description["path"].as_str())
                .unwrap_or("audio"),
            level(measurement.loudness.integrated_lufs, "LUFS"),
            level(measurement.levels.true_peak_dbtp, "dBTP"),
            measurement
                .loudness
                .loudness_range_lu
                .map_or_else(|| "unavailable".to_owned(), |v| format!("{v:.1} LU")),
        );
        if let Some(track) = &pitch_track {
            let s = &track.summary;
            summary.push_str(&match (s.median_hz, &s.median_note_name, s.median_note_cents) {
                (Some(hz), Some(name), Some(cents)) => format!(
                    ", median f0 {hz:.1} Hz ({name} {cents:+.0} cents) in {} of {} windows, {} note segments",
                    s.voiced_windows,
                    s.windows,
                    track.notes.len()
                ),
                _ => format!(", no voiced pitch window of {}", s.windows),
            });
            if track.short_runs_omitted > 0 {
                summary.push_str(&format!(
                    " ({} run(s) shorter than {} windows omitted)",
                    track.short_runs_omitted, track.settings.min_note_windows
                ));
            }
            let flagged = track
                .notes
                .iter()
                .filter(|n| !n.low_confidence.is_empty())
                .count();
            if flagged > 0 {
                summary.push_str(&format!(", {flagged} marked low_confidence"));
            }
        }
        let mut result = serde_json::to_value(&measurement).expect("serializes");
        if let Some(track) = pitch_track {
            result["pitch"] = serde_json::to_value(&track).expect("serializes");
        }
        result["input"] = loaded.description;
        result["conventions"] = json!(CONVENTIONS);
        done(ToolResponse::ok(summary, result))
    }

    pub(super) fn compare_audio(&mut self, input: CompareAudioInput) -> Handled {
        let max_offset = input.max_offset_seconds.unwrap_or(0.0);
        if !(0.0..=MAX_OFFSET_SECONDS).contains(&max_offset) {
            return done(ToolResponse::refused(
                Code::ValueOutOfRange,
                "/max_offset_seconds",
                format!("the offset search bound must be 0 to {MAX_OFFSET_SECONDS} s"),
            ));
        }
        let side_range = |own: Option<AudioRange>, pointer: &'static str| match own {
            Some(range) => (Some(range), pointer),
            None => (input.range, "/range"),
        };
        let reference = self.load_audio(
            &input.reference,
            side_range(input.reference_range, "/reference_range"),
            "/reference",
        )?;
        let candidate = self.load_audio(
            &input.candidate,
            side_range(input.candidate_range, "/candidate_range"),
            "/candidate",
        )?;
        let (r, c) = (&reference.format, &candidate.format);
        if r.sample_rate != c.sample_rate || r.channels != c.channels {
            return done(ToolResponse::refused(
                Code::ValueOutOfRange,
                "/candidate",
                format!(
                    "the candidate has {} channels at {} Hz but the reference has {} at {} Hz; Tunemend does not resample or remix for comparison",
                    c.channels, c.sample_rate, r.channels, r.sample_rate
                ),
            ));
        }
        let options = MeasureOptions {
            bands: input.spectrum,
            ..Default::default()
        };
        let reference_measurement = measure::measure(&reference.audio, &options);
        let candidate_measurement = measure::measure(&candidate.audio, &options);
        let offset = (max_offset * f64::from(r.sample_rate)).round() as usize;
        let comparison = measure::compare(
            (&reference.audio, &reference_measurement),
            (&candidate.audio, &candidate_measurement),
            offset,
            reference.start_seconds,
        );
        let pitch_comparison = match &input.pitch {
            Some(settings) => {
                let config = pitch_config(settings, &reference, measure::DEFAULT_SILENCE_DBFS)?;
                Some(pitch::compare(
                    (&reference.audio.channels, reference.start_seconds),
                    &candidate.audio.channels,
                    r.sample_rate,
                    &config,
                    comparison.difference.alignment.offset_samples,
                ))
            }
            None => None,
        };
        let difference = &comparison.difference;
        let mut summary = if difference.identical {
            "the candidate is sample-identical to the reference".to_owned()
        } else if difference.overlap_identical {
            format!(
                "overlap sample-identical ({} frames at offset {} samples); {} unmatched reference and {} unmatched candidate frames",
                difference.overlap_frames,
                difference.alignment.offset_samples,
                difference.unmatched_reference_frames,
                difference.unmatched_candidate_frames
            )
        } else {
            format!(
                "integrated {} LU, true peak {} dB, offset {} samples, residual {}",
                comparison
                    .deltas
                    .integrated_lu
                    .map_or("n/a".into(), |v| format!("{v:+.2}")),
                comparison
                    .deltas
                    .true_peak_db
                    .map_or("n/a".into(), |v| format!("{v:+.2}")),
                difference.alignment.offset_samples,
                difference
                    .overall
                    .residual_db
                    .map_or("n/a".into(), |v| format!("{v:.1} dB")),
            )
        };
        if let Some(p) = &pitch_comparison {
            summary.push_str(&match p.median_difference_cents {
                Some(cents) => format!(
                    "; pitch median {cents:+.1} cents over {} voiced windows, {} with another note",
                    p.compared_windows, p.different_note_windows
                ),
                None => "; no window voiced in both".to_owned(),
            });
        }
        let compact = |m: &measure::Measurement| {
            json!({"levels": m.levels, "loudness": m.loudness, "silence": m.silence,
                "frames": m.frames, "duration_seconds": m.duration_seconds})
        };
        let mut result = serde_json::to_value(&comparison).expect("serializes");
        result["reference"] =
            json!({"input": reference.description, "measurement": compact(&reference_measurement)});
        result["candidate"] =
            json!({"input": candidate.description, "measurement": compact(&candidate_measurement)});
        if let Some(p) = pitch_comparison {
            result["pitch"] = serde_json::to_value(&p).expect("serializes");
        }
        result["conventions"] = json!(CONVENTIONS);
        done(ToolResponse::ok(summary, result))
    }
}
