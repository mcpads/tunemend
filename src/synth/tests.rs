//! Synthesizer checks on SoundFonts built here, byte by byte.

use std::sync::Arc;

use super::*;

const RATE: u32 = 44_100;

/// A zone: generators (operator, amount) in file order and modulators
/// (source, destination, amount, amount source, transform).
#[derive(Clone, Default)]
struct ZoneSpec {
    generators: Vec<(u16, u16)>,
    modulators: Vec<[u16; 5]>,
}

fn zone(generators: &[(u16, i32)]) -> ZoneSpec {
    ZoneSpec {
        generators: generators
            .iter()
            .map(|&(o, a)| (o, a as i16 as u16))
            .collect(),
        modulators: vec![],
    }
}

fn range(low: u8, high: u8) -> i32 {
    i32::from(u16::from_le_bytes([low, high]))
}

struct SampleSpec {
    points: Vec<i16>,
    loop_start: u32,
    loop_end: u32,
    rate: u32,
    root: u8,
    kind: u16,
}

/// A looped sine-like cycle: `cycles` repetitions of a 64-point triangle,
/// root 69 at 64 × 440 Hz.
fn cycle_sample(cycles: usize) -> SampleSpec {
    let mut points = Vec::new();
    for i in 0..64 * cycles {
        let phase = (i % 64) as f64 / 64.0;
        let tri = if phase < 0.5 {
            4.0 * phase - 1.0
        } else {
            3.0 - 4.0 * phase
        };
        points.push((tri * 16000.0) as i16);
    }
    let length = points.len() as u32;
    SampleSpec {
        points,
        loop_start: 64,
        loop_end: length - 64,
        rate: 440 * 64,
        root: 69,
        kind: 1,
    }
}

struct FontSpec {
    version: (u16, u16),
    samples: Vec<SampleSpec>,
    sm24: Option<Vec<u8>>,
    instruments: Vec<Vec<ZoneSpec>>,
    /// (bank, program, zones).
    presets: Vec<(u16, u16, Vec<ZoneSpec>)>,
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

fn name(text: &str) -> Vec<u8> {
    let mut field = text.as_bytes().to_vec();
    field.resize(20, 0);
    field
}

/// Bags, generators and modulators of a list of zone lists.
fn hydra(zones: &[&[ZoneSpec]]) -> (Vec<u16>, Vec<u8>, Vec<u8>, Vec<u8>) {
    let (mut first_bags, mut bags, mut generators, mut modulators) =
        (vec![], vec![], vec![], vec![]);
    let (mut g, mut m) = (0u16, 0u16);
    for list in zones {
        first_bags.push((bags.len() / 4) as u16);
        for z in list.iter() {
            bags.extend(g.to_le_bytes());
            bags.extend(m.to_le_bytes());
            for (operator, amount) in &z.generators {
                generators.extend(operator.to_le_bytes());
                generators.extend(amount.to_le_bytes());
                g += 1;
            }
            for modulator in &z.modulators {
                for field in modulator {
                    modulators.extend(field.to_le_bytes());
                }
                m += 1;
            }
        }
    }
    first_bags.push((bags.len() / 4) as u16);
    bags.extend(g.to_le_bytes());
    bags.extend(m.to_le_bytes());
    generators.extend([0; 4]);
    modulators.extend([0; 10]);
    (first_bags, bags, generators, modulators)
}

fn build(spec: &FontSpec) -> Vec<u8> {
    let mut data: Vec<i16> = Vec::new();
    let mut headers = Vec::new();
    for (i, s) in spec.samples.iter().enumerate() {
        let start = data.len() as u32;
        data.extend(&s.points);
        let end = data.len() as u32;
        data.extend([0; 46]);
        let mut h = name(&format!("sample{i}"));
        for v in [start, end, start + s.loop_start, start + s.loop_end, s.rate] {
            h.extend(v.to_le_bytes());
        }
        h.extend([s.root, 0]);
        h.extend(0u16.to_le_bytes());
        h.extend(s.kind.to_le_bytes());
        headers.push(h);
    }
    let mut terminal = name("EOS");
    terminal.resize(46, 0);
    headers.push(terminal);

    let instrument_lists: Vec<&[ZoneSpec]> =
        spec.instruments.iter().map(|z| z.as_slice()).collect();
    let (inst_bags, ibag, igen, imod) = hydra(&instrument_lists);
    let mut inst = Vec::new();
    for (i, bag) in inst_bags.iter().enumerate() {
        inst.extend(name(if i + 1 == inst_bags.len() {
            "EOI"
        } else {
            "inst"
        }));
        inst.extend(bag.to_le_bytes());
    }
    let preset_lists: Vec<&[ZoneSpec]> =
        spec.presets.iter().map(|(_, _, z)| z.as_slice()).collect();
    let (preset_bags, pbag, pgen, pmod) = hydra(&preset_lists);
    let mut phdr = Vec::new();
    for (i, bag) in preset_bags.iter().enumerate() {
        let (bank, program) = spec.presets.get(i).map_or((0, 0), |p| (p.0, p.1));
        phdr.extend(name(if i + 1 == preset_bags.len() {
            "EOP"
        } else {
            "preset"
        }));
        phdr.extend(program.to_le_bytes());
        phdr.extend(bank.to_le_bytes());
        phdr.extend(bag.to_le_bytes());
        phdr.extend([0; 12]);
    }
    let smpl: Vec<u8> = data.iter().flat_map(|s| s.to_le_bytes()).collect();
    let mut sdta = vec![chunk(b"smpl", &smpl)];
    if let Some(sm24) = &spec.sm24 {
        sdta.push(chunk(b"sm24", sm24));
    }
    let info = list(
        b"INFO",
        &[
            chunk(
                b"ifil",
                &[spec.version.0.to_le_bytes(), spec.version.1.to_le_bytes()].concat(),
            ),
            chunk(b"INAM", b"test\0\0"),
        ],
    );
    let pdta = list(
        b"pdta",
        &[
            chunk(b"phdr", &phdr),
            chunk(b"pbag", &pbag),
            chunk(b"pmod", &pmod),
            chunk(b"pgen", &pgen),
            chunk(b"inst", &inst),
            chunk(b"ibag", &ibag),
            chunk(b"imod", &imod),
            chunk(b"igen", &igen),
            chunk(b"shdr", &headers.concat()),
        ],
    );
    chunk(
        b"RIFF",
        &[b"sfbk".to_vec(), info, list(b"sdta", &sdta), pdta].concat(),
    )
}

/// One looped sample, one instrument zone with `generators` (sampleID
/// appended), one preset 0:0.
/// The zone loops continuously unless `generators` sets sampleModes.
fn simple_font(generators: &[(u16, i32)]) -> FontSpec {
    let mut generators = [&[(54, 1)], generators].concat();
    generators.push((53, 0));
    FontSpec {
        version: (2, 4),
        samples: vec![cycle_sample(40)],
        sm24: None,
        instruments: vec![vec![zone(&generators)]],
        presets: vec![(0, 0, vec![zone(&[(41, 0)])])],
    }
}

fn synth(spec: &FontSpec, effects: bool) -> Synthesizer {
    let font = SoundFont::parse(&build(spec)).unwrap();
    Synthesizer::new(Arc::new(font), RATE, effects).unwrap()
}

fn run(s: &mut Synthesizer, samples: usize) -> (Vec<f32>, Vec<f32>) {
    let (mut l, mut r) = (vec![0.0; samples], vec![0.0; samples]);
    s.render(&mut l, &mut r);
    (l, r)
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
}

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0, |p, s| p.max(s.abs()))
}

#[test]
fn the_reference_soundfont_parses() {
    let font = SoundFont::parse(&crate::soundfont::reference_soundfont()).unwrap();
    assert_eq!(font.version, (2, 1));
    assert_eq!(font.name, "Tunemend reference waveforms");
    assert_eq!(font.presets().len(), 128);
    assert_eq!(font.instruments().len(), 4);
    assert_eq!(font.samples().len(), 4);
    assert_eq!(font.preset_index(0, 80), Some(80));
    let regions = font.regions(80, 60, 100);
    assert_eq!(regions.len(), 1);
    assert_eq!(regions[0].generators[34], -11959);
    assert_eq!(regions[0].generators[54], 1);
    assert_eq!(
        regions[0].modulators.len(),
        modulator::DEFAULT_MODULATORS.len()
    );
}

#[test]
fn structural_defects_are_refused_with_their_location() {
    let good = build(&simple_font(&[]));
    assert!(SoundFont::parse(&good).is_ok());

    let error = SoundFont::parse(&good[..good.len() - 10]).unwrap_err();
    assert_eq!(error.location, "RIFF");
    assert_eq!(error.offset, 4);

    let mut bad = good.clone();
    bad[8..12].copy_from_slice(b"WAVE");
    assert_eq!(
        SoundFont::parse(&bad).unwrap_err().message,
        "not a RIFF sfbk file"
    );

    // A sample end past the sample data.
    let mut spec = simple_font(&[]);
    spec.samples[0].loop_end = 10;
    let mut bytes = build(&spec);
    let shdr = bytes.windows(4).rposition(|w| w == b"shdr").unwrap() + 8;
    bytes[shdr + 24..shdr + 28].copy_from_slice(&99_999u32.to_le_bytes());
    let error = SoundFont::parse(&bytes).unwrap_err();
    assert_eq!(error.location, "pdta/shdr record 0");
    assert_eq!(error.offset, shdr);

    // A preset bag index below the previous one.
    let mut spec = simple_font(&[]);
    spec.presets.push((0, 1, vec![zone(&[(41, 0)])]));
    let mut bytes = build(&spec);
    let phdr = bytes.windows(4).position(|w| w == b"phdr").unwrap() + 8;
    bytes[phdr + 38 + 24..phdr + 38 + 26].copy_from_slice(&0u16.to_le_bytes());
    bytes[phdr + 24..phdr + 26].copy_from_slice(&1u16.to_le_bytes());
    let error = SoundFont::parse(&bytes).unwrap_err();
    assert_eq!(error.location, "pdta/phdr record 1", "{error}");

    // An instrument index past the instrument list.
    let mut spec = simple_font(&[]);
    spec.presets[0].2 = vec![zone(&[(41, 3)])];
    let error = SoundFont::parse(&build(&spec)).unwrap_err();
    assert!(error.location.starts_with("pdta/pgen"), "{error}");

    // A ROM sample.
    let mut spec = simple_font(&[]);
    spec.samples[0].kind = 0x8001;
    assert!(
        SoundFont::parse(&build(&spec))
            .unwrap_err()
            .message
            .contains("ROM")
    );

    // A hydra record size that is not a multiple of its record size.
    let mut bytes = good.clone();
    let pbag = bytes.windows(4).position(|w| w == b"pbag").unwrap();
    bytes[pbag..pbag + 4].copy_from_slice(b"pbaq");
    let error = SoundFont::parse(&bytes).unwrap_err();
    assert_eq!(error.location, "pdta/pbaq");
    assert_eq!(error.offset, pbag);
}

#[test]
fn twenty_four_bit_samples_are_combined_only_for_2_04() {
    let mut spec = simple_font(&[]);
    let points = spec.samples[0].points.len() + 46;
    spec.sm24 = Some(vec![0x80; points + points % 2]);
    let font = SoundFont::parse(&build(&spec)).unwrap();
    let first = f32::from(spec.samples[0].points[0]);
    assert_eq!(font.data[0], (first * 256.0 + 128.0) / 8_388_608.0);
    spec.version = (2, 1);
    let font = SoundFont::parse(&build(&spec)).unwrap();
    assert_eq!(font.data[0], first / 32_768.0);
}

#[test]
fn generators_follow_precedence_and_ranges() {
    let spec = FontSpec {
        version: (2, 4),
        samples: vec![cycle_sample(40)],
        sm24: None,
        instruments: vec![vec![
            // Global: attenuation 100, attack 0 timecents, sampleModes in a
            // global zone is still an instrument-level generator.
            zone(&[(48, 100), (34, 0)]),
            zone(&[(43, range(0, 59)), (48, 200), (53, 0)]),
            zone(&[(43, range(60, 127)), (44, range(0, 63)), (53, 0)]),
            zone(&[
                (43, range(60, 127)),
                (44, range(64, 127)),
                (51, 12),
                (53, 0),
            ]),
        ]],
        presets: vec![(
            0,
            0,
            vec![
                // Preset global: relative attenuation +50 and an ignored
                // sample generator.
                zone(&[(48, 50), (54, 1)]),
                zone(&[(43, range(50, 100)), (34, -1200), (41, 0)]),
            ],
        )],
    };
    let font = SoundFont::parse(&build(&spec)).unwrap();
    assert!(
        font.regions(0, 40, 100).is_empty(),
        "outside the preset range"
    );
    let low = font.regions(0, 55, 100);
    assert_eq!(low.len(), 1);
    assert_eq!(
        low[0].generators[48], 250,
        "local replaces global, preset adds"
    );
    assert_eq!(low[0].generators[34], -1200, "0 + -1200");
    assert_eq!(
        low[0].generators[54], 0,
        "preset sample generators are ignored"
    );
    let soft = font.regions(0, 70, 30);
    assert_eq!(
        (soft.len(), soft[0].generators[48], soft[0].generators[51]),
        (1, 150, 0)
    );
    let loud = font.regions(0, 70, 90);
    assert_eq!((loud.len(), loud[0].generators[51]), (1, 12));
    assert!(font.regions(0, 101, 90).is_empty());
}

#[test]
fn modulators_replace_defaults_and_preset_modulators_add() {
    let mut spec = simple_font(&[]);
    // Instrument: velocity → attenuation with amount 0 replaces the default.
    spec.instruments[0][0]
        .modulators
        .push([0x0502, 48, 0, 0, 0]);
    // Preset: CC1 → vibrato depth adds 25 to the default 50.
    spec.presets[0].2[0].modulators.push([0x0081, 6, 25, 0, 0]);
    // An illegal source (CC6, data entry) is ignored.
    spec.presets[0].2[0]
        .modulators
        .push([0x0086, 48, 960, 0, 0]);
    let font = SoundFont::parse(&build(&spec)).unwrap();
    let region = &font.regions(0, 60, 100)[0];
    let find = |source: u16, dest: u16| {
        region
            .modulators
            .iter()
            .find(|m| m.source == source && m.destination == dest)
            .map(|m| m.amount)
    };
    assert_eq!(find(0x0502, 48), Some(0));
    assert_eq!(find(0x0081, 6), Some(75));
    assert_eq!(find(0x0086, 48), None);
    assert_eq!(region.modulators.len(), modulator::DEFAULT_MODULATORS.len());

    // Rendered: with the replacement, velocity no longer changes the level.
    let mut a = synth(&spec, false);
    let mut b = synth(&spec, false);
    a.note_on(0, 69, 127);
    b.note_on(0, 69, 20);
    let (la, _) = run(&mut a, 4410);
    let (lb, _) = run(&mut b, 4410);
    // Only the velocity → filter modulator differs; levels match closely.
    assert!((rms(&la[2000..]) / rms(&lb[2000..]) - 1.0).abs() < 0.05);
}

#[test]
fn notes_start_on_their_sample_and_render_deterministically() {
    let spec = simple_font(&[]);
    let render = || {
        let mut s = synth(&spec, true);
        run(&mut s, 100);
        s.note_on(0, 69, 100);
        s.control_change(0, 91, 40);
        let (l, r) = run(&mut s, 30_000);
        (l, r)
    };
    let (l, r) = render();
    assert_eq!((l.clone(), r.clone()), render());
    assert!(l[0] != 0.0, "the first sample after note-on sounds");
    // Key 69 on a 440 Hz root plays 440 Hz.
    let crossings = (2000..24050)
        .filter(|&i| l[i - 1] <= 0.0 && l[i] > 0.0)
        .count();
    assert!((crossings as f64 / 0.5 - 440.0).abs() < 4.0, "{crossings}");
}

#[test]
fn volume_envelope_times_follow_the_generators() {
    // Delay 10 ms, attack 20 ms, hold 10 ms, decay to −20 dB at 100 dB/s,
    // release 1 s (100 dB per second).
    let spec = simple_font(&[
        (33, -7973),
        (34, -6773),
        (35, -7973),
        (36, 0),
        (37, 200),
        (38, 0),
    ]);
    let mut s = synth(&spec, false);
    s.note_on(0, 69, 127);
    let (l, _) = run(&mut s, 44_100);
    let ms = |m: f64| (m * 44.1) as usize;
    assert!(peak(&l[..ms(9.5)]) == 0.0, "delay");
    assert!(peak(&l[ms(10.5)..ms(12.0)]) > 0.0);
    let attack_mid = rms(&l[ms(19.0)..ms(21.0)]);
    let top = rms(&l[ms(31.0)..ms(39.0)]);
    assert!((attack_mid / top - 0.5).abs() < 0.08, "{attack_mid} {top}");
    // 100 ms into the decay: −10 dB.
    let decay = rms(&l[ms(139.0)..ms(141.0)]) / top;
    assert!((20.0 * decay.log10() + 10.0).abs() < 0.7, "{decay}");
    let sustain = rms(&l[ms(500.0)..ms(900.0)]) / top;
    assert!((20.0 * sustain.log10() + 20.0).abs() < 0.3, "{sustain}");
    s.note_off(0, 69);
    let (l, _) = run(&mut s, 22_050);
    let after = rms(&l[ms(195.0)..ms(205.0)]) / top;
    assert!(
        (20.0 * after.log10() + 40.0).abs() < 1.0,
        "200 ms of release: {after}"
    );
    assert_eq!(s.active_voices(), 1);
    run(&mut s, 22_050);
    assert_eq!(s.active_voices(), 0, "−100 dB ends the voice");
}

#[test]
fn loop_modes_play_through_or_repeat() {
    // Release takes one second, so only the sample end can stop a voice soon.
    let level = |mode: i32| {
        let mut s = synth(&simple_font(&[(54, mode), (38, 0)]), false);
        s.note_on(0, 69, 127);
        let (mut l, _) = run(&mut s, 10_000);
        s.note_off(0, 69);
        l.extend(run(&mut s, 10_000).0);
        (l, s.active_voices())
    };
    // The sample is 40 cycles at 440 Hz: about 4010 output samples.
    let (unlooped, voices) = level(0);
    assert!(rms(&unlooped[1000..3900]) > 0.1);
    assert_eq!(
        peak(&unlooped[4100..]),
        0.0,
        "mode 0 stops at the sample end"
    );
    assert_eq!(voices, 0);
    let (looped, voices) = level(1);
    assert!(rms(&looped[8000..10_000]) > 0.1, "mode 1 loops while held");
    assert!(rms(&looped[18_000..]) > 0.01, "and through the release");
    assert_eq!(voices, 1);
    // Mode 3: loops while held, then plays out the rest of the sample.
    let (sustained, voices) = level(3);
    assert!(rms(&sustained[8000..10_000]) > 0.1);
    assert!(
        rms(&sustained[10_000..10_200]) > 0.1,
        "plays on after release"
    );
    assert_eq!(peak(&sustained[14_100..]), 0.0, "and ends with the sample");
    assert_eq!(voices, 0);
}

#[test]
fn sustain_pedal_holds_released_keys() {
    let spec = simple_font(&[(38, -7973)]);
    let mut s = synth(&spec, false);
    s.control_change(0, 64, 127);
    s.note_on(0, 69, 100);
    run(&mut s, 1000);
    s.note_off(0, 69);
    let (held, _) = run(&mut s, 4410);
    assert!(rms(&held[3000..]) > 0.05, "pedal holds the note");
    s.control_change(0, 64, 0);
    let (released, _) = run(&mut s, 4410);
    assert!(rms(&released[2000..]) < 1e-6, "pedal up releases it");
    assert_eq!(s.active_voices(), 0);

    // Sostenuto holds only notes down when it is pressed.
    s.note_on(0, 60, 100);
    s.control_change(0, 66, 127);
    s.note_on(0, 72, 100);
    s.note_off(0, 60);
    s.note_off(0, 72);
    run(&mut s, 4410);
    assert_eq!(s.active_voices(), 1);
    s.control_change(0, 66, 0);
    run(&mut s, 4410);
    assert_eq!(s.active_voices(), 0);
}

#[test]
fn channel_mode_messages() {
    let spec = simple_font(&[(38, 0)]);
    let mut s = synth(&spec, false);
    s.note_on(0, 60, 100);
    s.note_on(1, 60, 100);
    s.control_change(0, 120, 0);
    assert_eq!(
        s.active_voices(),
        1,
        "All Sound Off stops channel 0 at once"
    );
    s.control_change(1, 64, 127);
    s.control_change(1, 123, 0);
    run(&mut s, 100);
    assert!(!s.voices[0].released, "All Notes Off waits for the pedal");
    s.control_change(1, 1, 90);
    s.control_change(1, 7, 50);
    s.pitch_bend(1, 0);
    s.control_change(1, 121, 0);
    let c = &s.channels[1];
    assert_eq!(
        (
            c.controllers[1],
            c.controllers[64],
            c.controllers[7],
            c.pitch_wheel
        ),
        (0, 0, 50, 8192)
    );
    assert!(s.voices[0].released, "reset releases pedal-held notes");
}

#[test]
fn pitch_wheel_uses_the_rpn_range() {
    let spec = simple_font(&[]);
    let frequency = |range: Option<u8>| {
        let mut s = synth(&spec, false);
        if let Some(range) = range {
            for (c, v) in [(101, 0), (100, 0), (6, range), (38, 0)] {
                s.control_change(0, c, v);
            }
        }
        s.pitch_bend(0, 8192 + 4096);
        s.note_on(0, 69, 100);
        let (l, _) = run(&mut s, 44_100);
        (1..44_100)
            .filter(|&i| l[i - 1] <= 0.0 && l[i] > 0.0)
            .count() as f64
    };
    // Half the range up: one semitone for ±2, six for ±12.
    assert!((frequency(None) / (440.0 * 2f64.powf(1.0 / 12.0)) - 1.0).abs() < 0.005);
    assert!((frequency(Some(12)) / (440.0 * 2f64.powf(0.5)) - 1.0).abs() < 0.005);
}

#[test]
fn pan_is_equal_power_and_follows_cc10() {
    let spec = simple_font(&[]);
    let sides = |pan: u8| {
        let mut s = synth(&spec, false);
        s.control_change(0, 10, pan);
        s.note_on(0, 69, 100);
        let (l, r) = run(&mut s, 4410);
        (rms(&l[1000..]), rms(&r[1000..]))
    };
    let (l, r) = sides(64);
    assert!((l / r - 1.0).abs() < 1e-6);
    let (hard_l, hard_r) = sides(0);
    assert_eq!(hard_r, 0.0);
    assert!((hard_l / l - 2f64.sqrt()).abs() < 1e-3, "equal power");
    let (l, r) = sides(96);
    assert!((l * l + r * r) / (hard_l * hard_l) - 1.0 < 1e-3);
    assert!(r > l);
}

#[test]
fn quiet_voices_still_reach_reverb_and_chorus() {
    let spec = simple_font(&[]);
    let render = |effects: bool, controller: u8, send: u8| {
        let mut s = synth(&spec, effects);
        s.control_change(0, 7, 20);
        s.control_change(0, 11, 30);
        s.control_change(0, controller, send);
        s.note_on(0, 69, 30);
        let (mut l, _) = run(&mut s, 4410);
        s.note_off(0, 69);
        l.extend(run(&mut s, 22_050).0);
        l
    };
    let dry = render(false, 91, 64);
    assert!(peak(&dry) < 2e-3, "the voice is quiet: {}", peak(&dry));
    assert_eq!(render(true, 91, 0), dry, "no send, no change");
    assert_eq!(render(true, 93, 0), dry);
    for controller in [91, 93] {
        let wet = render(true, controller, 64);
        assert_ne!(wet, dry, "CC{controller} 64 is heard");
        // After the 1 ms release, the effect still sounds: chorus for its
        // delay, reverb much longer.
        assert!(peak(&wet[4410 + 100..4410 + 600]) > 0.0, "CC{controller}");
    }
    assert!(
        peak(&render(true, 91, 64)[4410 + 4410..]) > 0.0,
        "reverb tail"
    );
}

#[test]
fn exclusive_classes_cut_each_other() {
    let mut spec = simple_font(&[(57, 1)]);
    spec.instruments[0][0]
        .generators
        .insert(0, (43, u16::from_le_bytes([0, 127])));
    let mut s = synth(&spec, false);
    s.note_on(9, 42, 100);
    run(&mut s, 1000);
    s.note_on(9, 46, 100);
    run(&mut s, 1000);
    assert_eq!(s.active_voices(), 1, "the open hi-hat cuts the closed one");
    assert_eq!(s.voices[0].key, 46);
}

#[test]
fn voices_beyond_the_limit_replace_released_then_oldest() {
    let spec = simple_font(&[(38, 2400)]);
    let mut s = synth(&spec, false);
    for key in 0..MAX_VOICES as u8 {
        s.note_on(0, key, 100);
    }
    s.note_off(0, 5);
    run(&mut s, 10);
    s.note_on(0, 100, 100);
    assert_eq!(s.active_voices(), MAX_VOICES);
    assert!(
        s.voices.iter().all(|v| v.key != 5),
        "the released voice goes first"
    );
    s.note_on(0, 101, 100);
    assert!(s.voices.iter().all(|v| v.key != 0), "then the oldest");
    assert_eq!(s.voices.last().unwrap().key, 101);
}

#[test]
fn programs_banks_and_percussion_select_presets() {
    let mut spec = simple_font(&[]);
    spec.instruments.push(vec![zone(&[(51, 12), (53, 0)])]);
    spec.instruments.push(vec![zone(&[(51, 24), (53, 0)])]);
    spec.presets.push((8, 0, vec![zone(&[(41, 1)])]));
    spec.presets.push((128, 0, vec![zone(&[(41, 2)])]));
    let mut s = synth(&spec, false);
    assert_eq!(s.channels[0].preset, Some(0));
    assert_eq!(s.channels[9].preset, Some(2), "channel 10 uses bank 128");
    s.control_change(0, 0, 8);
    assert_eq!(
        s.channels[0].preset,
        Some(0),
        "bank applies at the next program change"
    );
    s.program_change(0, 0);
    assert_eq!(s.channels[0].preset, Some(1));
    s.program_change(0, 5);
    assert_eq!(
        s.channels[0].preset,
        Some(0),
        "unknown program: first preset"
    );
    s.control_change(9, 0, 8);
    s.program_change(9, 3);
    assert_eq!(s.channels[9].preset, Some(2), "bank 128 program 0 fallback");
}
