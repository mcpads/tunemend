//! Auditions a static OPN patch through the built-in FM model with declared context.
use super::*;
use crate::fm::{FmPatch, OpnChip};
use crate::opn::{OpnAuditionContext, OpnNote, audition, resample, wav16};

const DEFAULT_TAIL_SECONDS: f64 = 3.0;

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenderFmPatchInput {
    pub job_id: String,
    /// Saved FM bank with `index`, or explicit `registers`; exactly one source.
    #[serde(default)]
    pub bank: Option<String>,
    #[serde(default)]
    pub index: Option<usize>,
    /// Explicit B0h and register slots 0,4,8,12 for groups 30h..90h.
    #[serde(default)]
    pub registers: Option<FmPatch>,
    /// Chip, clock, LFO, pan, AMS/PMS and key-on operators; nothing is inferred.
    pub context: OpnAuditionContext,
    /// 1..256 notes; notes on one channel must not overlap.
    pub notes: Vec<OpnNote>,
    /// Release time rendered after the last key-off, stopping early when every
    /// carrier is silent. Default 3, at most 30.
    #[serde(default)]
    pub tail_seconds: Option<f64>,
    /// Optional additional WAV at this rate (8000..192000), made by a reported
    /// windowed-sinc resampling step. The native-rate WAV is always written.
    #[serde(default)]
    pub output_sample_rate: Option<u32>,
    /// New folder relative to the workspace, such as `out/patch-12-audition`.
    pub destination: String,
}

fn name(chip: OpnChip) -> String {
    json!(chip).as_str().unwrap_or_default().to_owned()
}

fn refused(message: impl Into<String>) -> ToolResponse {
    ToolResponse::refused(Code::TargetRefused, "/context", message)
}

/// A patch with the chip that plays it, its source description and which
/// declaration set the chip.
pub(super) struct FmSource {
    pub patch: FmPatch,
    pub chip: OpnChip,
    pub source: Value,
    pub chip_authority: &'static str,
}

impl Service {
    pub(super) fn render_fm_patch(&mut self, input: RenderFmPatchInput) -> Handled {
        let FmSource {
            patch,
            chip,
            source,
            chip_authority,
        } = self.fm_source(
            input.bank.as_deref(),
            input.index,
            input.registers.as_ref(),
            input.context.chip,
        )?;
        if let Some(rate) = input.output_sample_rate
            && !(8_000..=192_000).contains(&rate)
        {
            return Err(refused("output_sample_rate must be 8000..192000"));
        }
        let tail = input.tail_seconds.unwrap_or(DEFAULT_TAIL_SECONDS);
        let rendered =
            audition(&patch, chip, &input.context, &input.notes, tail).map_err(refused)?;
        let header_rate = rendered.native_rate.round() as u32;
        let mut report = rendered.report;
        report["source"] = source;
        report["chip_authority"] = json!(chip_authority);
        report["tail_seconds"] = json!(tail);
        let mut files = vec![(
            "audition.wav".to_owned(),
            wav16(header_rate, &rendered.frames),
        )];
        if let Some(rate) = input.output_sample_rate {
            let (frames, clipped) = resample(&rendered.frames, rendered.native_rate, rate);
            report["resampled"] = json!({"file":format!("audition-{rate}.wav"),"sample_rate":rate,
                "from_rate_hz":rendered.native_rate,"method":"Blackman-windowed sinc, 32 zero crossings per side, cutoff 0.45 of the lower rate",
                "frames":frames.len(),"clipped_samples":clipped});
            files.push((format!("audition-{rate}.wav"), wav16(rate, &frames)));
        }
        report["files"] = json!({"native":"audition.wav","report":"audition.json"});
        files.push((
            "audition.json".to_owned(),
            serde_json::to_vec_pretty(&report).expect("serializes"),
        ));
        let mut response = self.export_sound_files(&input.job_id, &input.destination, files)?;
        response.summary = format!(
            "rendered {} note(s) with the Tunemend {} FM model at {:.2} Hz",
            input.notes.len(),
            name(chip),
            rendered.native_rate
        );
        report["readback_checked"] = json!(true);
        report["hardware_audio"] = json!(false);
        response.result = report;
        done(response)
    }

    /// The patch of a saved FM bank record or explicit registers, and the chip
    /// from the bank's declared model or the context. Refusals locate `/context`.
    pub(super) fn fm_source(
        &mut self,
        bank: Option<&str>,
        index: Option<usize>,
        registers: Option<&FmPatch>,
        context_chip: Option<OpnChip>,
    ) -> Result<FmSource, ToolResponse> {
        let (patch, bank_chip, source) = match (bank, index, registers) {
            (Some(id), Some(index), None) => {
                let bank = self.saved_fm_bank(id)?;
                let record = bank.inspect(index).map_err(refused)?;
                let patch = bank.patch(index).map_err(refused)?;
                let source = json!({"kind":"fm_bank","bank":id,"index":index,
                    "file_offset":record["file_offset"],"raw_record":record["raw_record"],
                    "unmodeled_register_bits":record["unmodeled_register_bits"],"bank_chip":bank.chip});
                (patch, Some(bank.chip), source)
            }
            (None, None, Some(registers)) => (
                registers.clone(),
                None,
                json!({"kind":"explicit_registers"}),
            ),
            _ => {
                return Err(refused(
                    "provide bank with index, or explicit registers, exactly one",
                ));
            }
        };
        let chip = match (bank_chip, context_chip) {
            (Some(OpnChip::Opn) | None, Some(chip)) if chip != OpnChip::Opn => chip,
            (Some(bank), None) if bank != OpnChip::Opn => bank,
            (Some(bank), Some(chip)) if bank == chip => chip,
            (Some(bank), Some(chip)) if bank != OpnChip::Opn => {
                return Err(refused(format!(
                    "the bank declares {} but the context declares {}; copy the patch into a bank of that chip instead",
                    name(bank),
                    name(chip)
                )));
            }
            _ => {
                return Err(refused(
                    "declare context.chip as ym2203, ym2608 or ym2612; a family-only patch has no playback model",
                ));
            }
        };
        Ok(FmSource {
            patch,
            chip,
            source,
            chip_authority: if bank_chip.is_some_and(|c| c != OpnChip::Opn) {
                "bank declaration"
            } else {
                "context declaration"
            },
        })
    }
}
