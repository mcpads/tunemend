//! Resolving voice instrument assignments against saved banks and plans.
use super::*;
use crate::mix::Mix;
use crate::render::ChipInstruments;
use crate::voice_instruments::{
    Chip, ChipInstrument, FmChip, InstrumentAssignment, SNES_VOICES, SnesChip, SnesPitch,
    SnesPitchMap, VoiceInstrument,
};

/// Moves a refusal's diagnostic locations under `prefix`.
fn located(mut response: ToolResponse, prefix: &str) -> ToolResponse {
    for diagnostic in &mut response.diagnostics {
        diagnostic.location = format!("{prefix}{}", diagnostic.location);
    }
    response
}

fn refused(location: String, message: impl Into<String>) -> ToolResponse {
    ToolResponse::refused(Code::TargetRefused, &location, message)
}

impl Service {
    /// The instruments of the voices a render plays (`voices`, or every
    /// voice), loaded from saved data whose identities are checked again.
    /// `base` locates the mixer, such as `/mix`.
    pub(super) fn chip_instruments(
        &mut self,
        mix: &Mix,
        voices: Option<&[String]>,
        base: &str,
    ) -> Result<ChipInstruments, ToolResponse> {
        let mut resolved = ChipInstruments::new();
        for (index, assignment) in mix.instruments.iter().enumerate() {
            if voices.is_some_and(|v| !v.contains(&assignment.voice)) {
                continue;
            }
            let instrument = self.chip_instrument(index, assignment, base)?;
            resolved.insert(assignment.voice.clone(), instrument);
        }
        Ok(resolved)
    }

    fn chip_instrument(
        &mut self,
        index: usize,
        assignment: &InstrumentAssignment,
        base: &str,
    ) -> Result<ChipInstrument, ToolResponse> {
        let at = format!("{base}/instruments/{index}/instrument");
        match &assignment.instrument {
            VoiceInstrument::SnesBrr(i) => {
                let (brr, loop_offset, source, bank) = self
                    .brr_sample(i.bank.as_deref(), i.slot, i.plan.as_deref())
                    .map_err(|r| located(r, &at))?;
                crate::snes_dsp::check_sample(&brr, loop_offset)
                    .map_err(|e| refused(at.clone(), e))?;
                let pitch = match i.pitch {
                    SnesPitchMap::Root {
                        root_note,
                        root_register,
                    } => SnesPitch::Root(crate::snes_dsp::NoteRoot::Register {
                        root_note,
                        root_register,
                    }),
                    SnesPitchMap::PlanRoot => {
                        SnesPitch::Root(super::audition::plan_root(&source).ok_or_else(|| {
                            refused(
                                format!("{at}/pitch"),
                                "plan_root needs a sample plan prepared with a root; use root with an explicit root_register otherwise",
                            )
                        })?)
                    }
                    SnesPitchMap::Fixed { register } => SnesPitch::Fixed(register),
                };
                let (adsr1, adsr2, gain, envelope) =
                    crate::snes_dsp::envelope_registers(i.envelope, bank.as_ref())
                        .map_err(|e| refused(format!("{at}/envelope"), e))?;
                let mut defaults = Vec::new();
                let master = i.master_volume.unwrap_or_else(|| {
                    defaults.push("master_volume=127/127");
                    super::audition::master_volume()
                });
                let counter_start = i.counter_start.unwrap_or_else(|| {
                    defaults.push("counter_start=0");
                    0
                });
                let max_voices = i.max_voices.unwrap_or_else(|| {
                    defaults.push("max_voices=8");
                    SNES_VOICES
                });
                Ok(ChipInstrument {
                    assignment: index,
                    max_voices: max_voices.into(),
                    channel_controls: i.channel_controls,
                    report: json!({"kind": "snes_brr", "source": source, "envelope": envelope,
                        "pitch_map": i.pitch, "defaults_applied": defaults}),
                    chip: Chip::Snes(Box::new(SnesChip {
                        brr,
                        loop_offset,
                        pitch,
                        adsr1,
                        adsr2,
                        gain,
                        volume: [i.volume.left, i.volume.right],
                        master: [master.left, master.right],
                        velocity: i.velocity,
                        counter_start: counter_start.into(),
                    })),
                })
            }
            VoiceInstrument::Fm(i) => {
                let context = format!("{at}/context");
                let source = self
                    .fm_source(
                        i.bank.as_deref(),
                        i.index,
                        i.registers.as_ref(),
                        i.context.chip,
                    )
                    .map_err(|r| {
                        let mut r = r;
                        for d in &mut r.diagnostics {
                            d.location = context.clone();
                        }
                        r
                    })?;
                let setup = crate::opn::prepare(&source.patch, source.chip, &i.context)
                    .map_err(|e| refused(context.clone(), e))?;
                let channels = crate::opn::channel_count(source.chip);
                let max_voices = usize::from(i.max_voices.unwrap_or(channels as u8));
                if max_voices > channels {
                    return Err(refused(
                        format!("{at}/max_voices"),
                        format!("this chip has {channels} FM channels"),
                    ));
                }
                Ok(ChipInstrument {
                    assignment: index,
                    max_voices,
                    channel_controls: i.channel_controls,
                    report: json!({"kind": "fm", "source": source.source, "chip": source.chip,
                        "chip_authority": source.chip_authority,
                        "native_rate_exact": format!("{}/{}", setup.clock_hz, setup.divider),
                        "a4_hz": i.a4_hz, "block": i.block,
                        "defaults_applied": if i.max_voices.is_none() { json!([format!("max_voices={channels}")]) } else { json!([]) }}),
                    chip: Chip::Fm(Box::new(FmChip {
                        patch: source.patch,
                        setup,
                        a4_hz: i.a4_hz.value(),
                        block: i.block,
                        velocity: i.velocity,
                    })),
                })
            }
        }
    }

    /// How long each chip-instrument voice's notes sound after their end, for
    /// counting simultaneous notes: `declared_ms` when the profile declares
    /// it, otherwise the instrument's release envelope.
    pub(super) fn voice_releases(
        &mut self,
        mix: &Mix,
        declared_ms: Option<u32>,
    ) -> crate::profile::VoiceReleases {
        use crate::profile::VoiceRelease;
        let mut releases = crate::profile::VoiceReleases::new();
        for (index, assignment) in mix.instruments.iter().enumerate() {
            let release = if let Some(ms) = declared_ms {
                Ok(VoiceRelease {
                    seconds: Rational::new(ms.into(), 1000),
                    basis: "declared chip_release_ms".into(),
                })
            } else {
                match &assignment.instrument {
                    VoiceInstrument::SnesBrr(_) => Ok(VoiceRelease {
                        seconds: Rational::new(
                            crate::snes_dsp::RELEASE_BOUND_SAMPLES.into(),
                            crate::snes_dsp::SAMPLE_RATE.into(),
                        ),
                        basis: format!(
                            "S-DSP release: the envelope falls by 8 per DSP sample from the key-off poll, at most {} samples at 32000 Hz including the wait for the even-sample poll",
                            crate::snes_dsp::RELEASE_BOUND_SAMPLES
                        ),
                    }),
                    VoiceInstrument::Fm(_) => match self.chip_instrument(index, assignment, "/mix")
                    {
                        Ok(ChipInstrument {
                            chip: Chip::Fm(fm), ..
                        }) => {
                            let samples = crate::opn::release_bound_samples(&fm.patch);
                            Ok(VoiceRelease {
                                seconds: Rational::new(
                                    i128::from(samples) * i128::from(fm.setup.divider),
                                    fm.setup.clock_hz.into(),
                                ),
                                basis: format!(
                                    "OPN release: carriers from 0 to maximum attenuation at rate 4 x RR + 2 without key scaling, at most {samples} native samples"
                                ),
                            })
                        }
                        Ok(_) => Err("not an FM instrument".into()),
                        Err(response) => Err(format!(
                            "instrument not resolved: {}",
                            response
                                .diagnostics
                                .first()
                                .map_or(response.summary.clone(), |d| d.message.clone())
                        )),
                    },
                }
            };
            releases.insert(assignment.voice.clone(), release);
        }
        releases
    }

    /// Resolves instruments an edit added or changed, so unknown banks and
    /// plans refuse the edit instead of a later render.
    pub(super) fn check_changed_instruments(
        &mut self,
        base: &Mix,
        result: &Mix,
    ) -> Result<(), ToolResponse> {
        let changed =
            crate::voice_instruments::changed_voices(&base.instruments, &result.instruments);
        for (index, assignment) in result.instruments.iter().enumerate() {
            if changed.contains(&assignment.voice.as_str()) {
                self.chip_instrument(index, assignment, "/result/mix")
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
            }
        }
        Ok(())
    }
}
