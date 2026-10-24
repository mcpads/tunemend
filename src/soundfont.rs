//! The built-in reference sound source: simple looped waveforms written as a
//! SoundFont 2 file. It lets every installation hear MIDI structure without
//! downloading instruments; it does not imitate a game sound chip.

/// Name recorded on renders that use [`reference_soundfont`].
pub const REFERENCE_SOUND_SOURCE: &str = "reference-waveforms";

const CYCLE: usize = 64;
const CYCLES: usize = 8;
/// One 64-point cycle at this rate is 440 Hz, MIDI key 69.
const SAMPLE_RATE: u32 = 440 * CYCLE as u32;
const ROOT_KEY: u8 = 69;
const AMPLITUDE: f64 = 0.45 * i16::MAX as f64;

#[derive(Clone, Copy)]
enum Wave {
    Square,
    Pulse25,
    Triangle,
    Saw,
}

const WAVES: [(Wave, &str); 4] = [
    (Wave::Square, "square"),
    (Wave::Pulse25, "pulse25"),
    (Wave::Triangle, "triangle"),
    (Wave::Saw, "saw"),
];

impl Wave {
    fn point(self, phase: f64) -> f64 {
        match self {
            Wave::Square => {
                if phase < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            Wave::Pulse25 => {
                if phase < 0.25 {
                    1.0
                } else {
                    -1.0
                }
            }
            Wave::Triangle => {
                if phase < 0.5 {
                    4.0 * phase - 1.0
                } else {
                    3.0 - 4.0 * phase
                }
            }
            Wave::Saw => 2.0 * phase - 1.0,
        }
    }
}

/// Waveform index in [`WAVES`] used for a General MIDI program number.
pub fn waveform_for_program(program: u8) -> usize {
    match program {
        80 => 0,
        81 => 3,
        32..=39 | 8..=15 | 72..=79 | 112..=119 => 2,
        40..=71 | 88..=95 => 3,
        0..=7 | 24..=31 | 104..=111 => 1,
        _ => 0,
    }
}

/// Builds the reference SoundFont. The output is the same on every call.
pub fn reference_soundfont() -> Vec<u8> {
    let mut samples: Vec<i16> = Vec::new();
    let mut sample_headers = Vec::new();
    for (wave, name) in WAVES {
        let start = samples.len() as u32;
        for i in 0..CYCLE * CYCLES {
            let phase = (i % CYCLE) as f64 / CYCLE as f64;
            samples.push((wave.point(phase) * AMPLITUDE).round() as i16);
        }
        let end = samples.len() as u32;
        // The specification requires 46 zero points after each sample.
        samples.extend(std::iter::repeat_n(0, 46));
        let mut header = name_field(name);
        for value in [
            start,
            end,
            start + CYCLE as u32,
            end - CYCLE as u32,
            SAMPLE_RATE,
        ] {
            header.extend(value.to_le_bytes());
        }
        header.extend([ROOT_KEY, 0]);
        header.extend(0u16.to_le_bytes());
        header.extend(1u16.to_le_bytes()); // mono sample
        sample_headers.push(header);
    }
    let mut terminal_sample = name_field("EOS");
    terminal_sample.resize(46, 0);
    sample_headers.push(terminal_sample);

    // One instrument per waveform with a short envelope and a looped sample.
    let mut instruments = Vec::new();
    let mut instrument_bags = Vec::new();
    let mut instrument_generators = Vec::new();
    for (index, (_, name)) in WAVES.iter().enumerate() {
        instruments.push(
            [
                name_field(name),
                (instrument_bags.len() as u16).to_le_bytes().to_vec(),
            ]
            .concat(),
        );
        instrument_bags.push(bag(instrument_generators.len()));
        instrument_generators.extend([
            generator(34, -11959),       // attackVolEnv: 1 ms
            generator(38, -4870),        // releaseVolEnv: 60 ms
            generator(54, 1),            // sampleModes: loop continuously
            generator(53, index as i16), // sampleID, last in the zone
        ]);
    }
    instruments.push(
        [
            name_field("EOI"),
            (instrument_bags.len() as u16).to_le_bytes().to_vec(),
        ]
        .concat(),
    );
    instrument_bags.push(bag(instrument_generators.len()));
    instrument_generators.push(generator(0, 0));

    let mut presets = Vec::new();
    let mut preset_bags = Vec::new();
    let mut preset_generators = Vec::new();
    for program in 0..128u16 {
        let wave = waveform_for_program(program as u8);
        let mut header = name_field(&format!("{program:03} {}", WAVES[wave].1));
        header.extend(program.to_le_bytes());
        header.extend(0u16.to_le_bytes()); // bank
        header.extend((preset_bags.len() as u16).to_le_bytes());
        header.extend([0u8; 12]);
        presets.push(header);
        preset_bags.push(bag(preset_generators.len()));
        preset_generators.push(generator(41, wave as i16)); // instrument
    }
    let mut terminal_preset = name_field("EOP");
    terminal_preset.extend([0u8; 4]);
    terminal_preset.extend((preset_bags.len() as u16).to_le_bytes());
    terminal_preset.extend([0u8; 12]);
    presets.push(terminal_preset);
    preset_bags.push(bag(preset_generators.len()));
    preset_generators.push(generator(0, 0));

    let info = list(
        b"INFO",
        &[
            chunk(b"ifil", &[2u16.to_le_bytes(), 1u16.to_le_bytes()].concat()),
            chunk(b"isng", &zero_terminated("EMU8000")),
            chunk(b"INAM", &zero_terminated("Tunemend reference waveforms")),
        ],
    );
    let sample_data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    let sdta = list(b"sdta", &[chunk(b"smpl", &sample_data)]);
    let pdta = list(
        b"pdta",
        &[
            chunk(b"phdr", &presets.concat()),
            chunk(b"pbag", &preset_bags.concat()),
            chunk(b"pmod", &[0u8; 10]),
            chunk(b"pgen", &preset_generators.concat()),
            chunk(b"inst", &instruments.concat()),
            chunk(b"ibag", &instrument_bags.concat()),
            chunk(b"imod", &[0u8; 10]),
            chunk(b"igen", &instrument_generators.concat()),
            chunk(b"shdr", &sample_headers.concat()),
        ],
    );
    let body = [b"sfbk".to_vec(), info, sdta, pdta].concat();
    chunk(b"RIFF", &body)
}

fn name_field(name: &str) -> Vec<u8> {
    let mut field = name.as_bytes().to_vec();
    field.resize(20, 0);
    field
}

fn zero_terminated(text: &str) -> Vec<u8> {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(0);
    if bytes.len() % 2 == 1 {
        bytes.push(0);
    }
    bytes
}

fn bag(generator_index: usize) -> Vec<u8> {
    [(generator_index as u16).to_le_bytes(), 0u16.to_le_bytes()].concat()
}

fn generator(operator: u16, amount: i16) -> Vec<u8> {
    [operator.to_le_bytes(), amount.to_le_bytes()].concat()
}

fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = id.to_vec();
    out.extend((data.len() as u32).to_le_bytes());
    out.extend(data);
    if data.len() % 2 == 1 {
        out.push(0);
    }
    out
}

fn list(kind: &[u8; 4], chunks: &[Vec<u8>]) -> Vec<u8> {
    chunk(b"LIST", &[kind.to_vec(), chunks.concat()].concat())
}
