//! S-DSP voice auditions of retained BRR instruments and prepared samples.
use super::*;
pub use crate::snes_dsp::{DspEnvelope, StereoVolume};
use crate::snes_dsp::{NoteRoot, SAMPLE_RATE, VoiceSetup, audition, stereo_wav};
use std::collections::BTreeMap;

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenderInstrumentInput {
    pub job_id: String,
    /// Saved instrument bank; use with `slot`. Mutually exclusive with `plan`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bank: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<u8>,
    /// Saved sample plan with a BRR target; its target bytes and kept loop are played.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    pub pitch: DspPitch,
    pub envelope: DspEnvelope,
    /// DSP samples (32000 Hz nominal) from key-on until KOFF is written.
    pub key_on_samples: u32,
    /// DSP samples rendered after KOFF is written, for the release tail.
    pub release_samples: u32,
    /// VxVOLL/VxVOLR, signed. Default 64/64.
    #[serde(default = "voice_volume")]
    pub volume: StereoVolume,
    /// MVOLL/MVOLR, signed. Default 127/127.
    #[serde(default = "master_volume")]
    pub master_volume: StereoVolume,
    /// Global envelope counter at key-on, 0..0x77FF. Hardware does not expose
    /// it; it only shifts where rate-limited envelope steps land.
    #[serde(default)]
    pub counter_start: u16,
    /// Rate written in the WAV header, 31000..33000. Samples stay one per DSP
    /// tick and are never resampled; declare an observed DSP clock (such as an
    /// emucap capture's 32040) to compare with that capture. Default 32000.
    #[serde(default = "nominal_rate")]
    pub wav_sample_rate: u32,
    /// Optional new folder for `audition.wav` and `audition.json`, like
    /// render_fm_patch. The WAV is always retained in the store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
}

fn nominal_rate() -> u32 {
    SAMPLE_RATE
}

pub(super) fn voice_volume() -> StereoVolume {
    StereoVolume {
        left: 64,
        right: 64,
    }
}

pub(super) fn master_volume() -> StereoVolume {
    StereoVolume {
        left: 127,
        right: 127,
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DspPitch {
    /// VxPITCH register value 1..0x3FFF; 0x1000 plays one decoded sample per DSP sample.
    Register { value: u16 },
    /// Equal-tempered MIDI note relative to an explicit root: `root_register`
    /// is the pitch register value that sounds `root_note`.
    Note {
        note: u8,
        root_note: u8,
        root_register: u16,
    },
    /// Equal-tempered MIDI note against the root and declared rate recorded in
    /// the selected sample plan (prepare_sample `root`). Needs `plan`.
    PlanNote { note: u8 },
}

/// A sample plan record as stored by prepare_sample.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct PlanRecord {
    pub(super) files: BTreeMap<String, String>,
    pub(super) report: Value,
}

fn refused(location: &str, message: impl Into<String>) -> ToolResponse {
    ToolResponse::refused(Code::TargetRefused, location, message)
}

/// The plan's declared-rate register and root, from its tuning report.
pub(super) fn plan_root(source: &Value) -> Option<NoteRoot> {
    let tuning = source.get("tuning")?;
    Some(NoteRoot::Plan {
        declared_rate_register: tuning
            .pointer("/snes_pitch/original_pitch/exact")?
            .as_f64()?,
        root_note: tuning.pointer("/root/note")?.as_f64()?,
        root_cents: tuning.pointer("/root/cents")?.as_f64()?,
    })
}

fn pitch_register(pitch: DspPitch, source: &Value) -> Result<(u16, Value), ToolResponse> {
    let resolved = match pitch {
        DspPitch::PlanNote { note } => {
            let Some(root) = plan_root(source) else {
                return Err(refused(
                    "/pitch",
                    "plan_note needs a sample plan prepared with a root; use note with an explicit root_register otherwise",
                ));
            };
            crate::snes_dsp::note_register(note, root)
        }
        DspPitch::Register { value } => crate::snes_dsp::check_pitch_register(value as f64)
            .map(|()| (value, json!({"kind": "register"}))),
        DspPitch::Note {
            note,
            root_note,
            root_register,
        } => crate::snes_dsp::note_register(
            note,
            NoteRoot::Register {
                root_note,
                root_register,
            },
        ),
    };
    resolved.map_err(|e| refused("/pitch", e))
}

/// BRR bytes, kept loop offset, source description and the bank when one was selected.
pub(super) type BrrSample = (
    Vec<u8>,
    Option<usize>,
    Value,
    Option<crate::instruments::InstrumentBank>,
);

impl Service {
    /// The BRR sample of a saved bank slot or a sample plan with a BRR
    /// target, after checking the stored identities.
    pub(super) fn brr_sample(
        &mut self,
        bank: Option<&str>,
        slot: Option<u8>,
        plan: Option<&str>,
    ) -> Result<BrrSample, ToolResponse> {
        match (bank, slot, plan) {
            (Some(id), Some(slot), None) => {
                let (bank, _) = self.instrument_bank_data(id)?;
                let sample = bank.sample(slot).map_err(|e| refused("/slot", e))?;
                let loop_offset = if sample.looping {
                    sample.loop_offset
                } else {
                    None
                };
                let source = json!({"kind": "bank_slot", "bank": id, "slot": slot, "sample": sample.summary(),
                    "native_tuning_bytes": sample.tuning,
                    "tuning_applied": false});
                Ok((sample.bytes, loop_offset, source, Some(bank)))
            }
            (None, None, Some(id)) => {
                let record: PlanRecord = self
                    .shared_document("sample_plan", id)?
                    .ok_or_else(|| refused("/plan", "unknown sample plan"))?;
                let store = self.store_ref()?;
                if sha256_hex(&serde_json::to_vec(&record).expect("serializes")) != *id {
                    return Err(ToolResponse::error("sample plan identity mismatch"));
                }
                let conversion = &record.report["conversion"];
                if conversion["target"] != "brr" {
                    return Err(refused(
                        "/plan",
                        "the S-DSP plays BRR; prepare a plan with target brr",
                    ));
                }
                let hash = record
                    .files
                    .get("sample.bin")
                    .ok_or_else(|| ToolResponse::error("plan has no sample.bin"))?;
                let bytes = std::fs::read(store.object_path(hash))
                    .map_err(|e| ToolResponse::error(e.to_string()))?;
                if sha256_hex(&bytes) != *hash {
                    return Err(ToolResponse::error("sample object identity mismatch"));
                }
                let loop_offset = conversion
                    .pointer("/loop/target_loop_offset")
                    .and_then(Value::as_u64)
                    .map(|o| o as usize);
                let source = json!({"kind": "sample_plan", "plan": id, "sample_sha256": hash,
                    "bytes": bytes.len(), "loop_offset": loop_offset,
                    "loop_policy": record.report["loop_policy"],
                    "source_sample_rate": conversion["sample_rate"],
                    "sample_rate": conversion.get("target_sample_rate").unwrap_or(&conversion["sample_rate"]),
                    "tuning": conversion["tuning"]});
                Ok((bytes, loop_offset, source, None))
            }
            _ => Err(refused(
                "/bank",
                "select bank with slot, or plan, but not both",
            )),
        }
    }

    pub(super) fn render_instrument(&mut self, input: RenderInstrumentInput) -> Handled {
        let (brr, loop_offset, source, bank) =
            self.brr_sample(input.bank.as_deref(), input.slot, input.plan.as_deref())?;
        let (pitch, pitch_detail) = pitch_register(input.pitch, &source)?;
        let (adsr1, adsr2, gain, envelope_detail) =
            crate::snes_dsp::envelope_registers(input.envelope, bank.as_ref())
                .map_err(|e| refused("/envelope", e))?;
        if !(31_000..=33_000).contains(&input.wav_sample_rate) {
            return Err(refused(
                "/wav_sample_rate",
                "declare a DSP clock of 31000..33000 Hz; samples are never resampled",
            ));
        }
        if input.counter_start as u32 >= crate::snes_dsp::COUNTER_RANGE {
            return Err(refused("/counter_start", "counter_start is 0..0x77FF"));
        }
        let setup = VoiceSetup {
            brr,
            loop_offset,
            pitch,
            adsr1,
            adsr2,
            gain,
            volume: [input.volume.left, input.volume.right],
            master: [input.master_volume.left, input.master_volume.right],
            key_on_samples: input.key_on_samples,
            release_samples: input.release_samples,
            counter_start: input.counter_start as u32,
        };
        let rendered = audition(&setup).map_err(|e| refused("", e))?;
        let wav = stereo_wav(&rendered.frames, input.wav_sample_rate);
        let store = self.store_mut(true)?;
        let object = store.put_object(&wav)?;
        let path = store.object_path(&object);
        // Workspace-relative, as measure_audio and compare_audio take `path`.
        let measurable = path
            .strip_prefix(&self.workspace)
            .unwrap_or(&path)
            .display()
            .to_string();
        let mut response = ToolResponse::ok(
            format!(
                "S-DSP audition: {} DSP samples (WAV declares {} Hz), pitch 0x{pitch:04X}; measure it with {{\"audio\": {{\"path\": \"{measurable}\"}}}}",
                rendered.frames.len(),
                input.wav_sample_rate
            ),
            json!({"source": source, "pitch": pitch_detail, "envelope": envelope_detail,
                "dsp": rendered.report,
                "wav": {"sha256": object, "channels": 2, "bits": 16, "sample_rate": input.wav_sample_rate, "nominal_dsp_rate": SAMPLE_RATE, "frames": rendered.frames.len(), "resampled": false},
                "limits": "one S-DSP voice from documented hardware behavior; native driver pitch arithmetic, vibrato, volume programs, echo and the console's real sample clock are not applied"}),
        );
        response.artifacts.push(Artifact {
            kind: "wav".into(),
            path: path.display().to_string(),
            sha256: object,
        });
        if let Some(destination) = &input.destination {
            let report = serde_json::to_vec_pretty(&response.result).expect("serializes");
            let exported = self.export_sound_files(
                &input.job_id,
                destination,
                vec![
                    ("audition.wav".into(), wav),
                    ("audition.json".into(), report),
                ],
            )?;
            response.artifacts.extend(exported.artifacts);
        }
        // Paths locate files in this workspace, so they are in the response
        // and not in the exported audition.json.
        response.result["wav"]["path"] = json!(measurable);
        response.result["measure_with"] = json!({"audio": {"path": measurable, "expected_sha256": response.result["wav"]["sha256"]}});
        if let Some(destination) = &input.destination {
            response.result["exported"] = json!({
                "wav": Path::new(destination).join("audition.wav"),
                "report": Path::new(destination).join("audition.json"),
                "note": "workspace-relative paths; measure_audio and compare_audio take either the exported or the stored path"});
        }
        done(response)
    }
}
