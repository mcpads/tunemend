mod common;

use tunemend::render::{RenderedAudio, render, sample_at, wav_bytes};
use tunemend::review::RenderLayout;
use tunemend::soundfont::reference_soundfont;
use tunemend::timeline::Rational;

const RATE: u32 = 44_100;

fn rms(audio: &RenderedAudio, from_seconds: f64, to_seconds: f64) -> f64 {
    let (from, to) = (
        (from_seconds * RATE as f64) as usize,
        (to_seconds * RATE as f64) as usize,
    );
    let samples = &audio.left[from..to];
    (samples.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / samples.len() as f64).sqrt()
}

/// Frequency from rising zero crossings in a window.
fn frequency(audio: &RenderedAudio, from_seconds: f64, to_seconds: f64) -> f64 {
    let (from, to) = (
        (from_seconds * RATE as f64) as usize,
        (to_seconds * RATE as f64) as usize,
    );
    let crossings: Vec<usize> = (from + 1..to)
        .filter(|&i| audio.left[i - 1] <= 0.0 && audio.left[i] > 0.0)
        .collect();
    let span = (crossings[crossings.len() - 1] - crossings[0]) as f64 / RATE as f64;
    (crossings.len() - 1) as f64 / span
}

#[test]
fn the_reference_soundfont_is_stable_and_loads() {
    let font = reference_soundfont();
    assert_eq!(font, reference_soundfont());
    let parsed = tunemend::synth::SoundFont::parse(&font).unwrap();
    assert_eq!(parsed.presets().len(), 128);
    assert_eq!(parsed.instruments().len(), 4);
}

#[test]
fn effect_render_follows_pitch_bend_expression_and_release() {
    let bundle = common::chime();
    let chime = &bundle.revisions[0].composition;
    let audio = render(
        bundle.project.timebase,
        chime,
        &RenderLayout::Full { loop_passes: 1 },
        None,
        &reference_soundfont(),
        RATE,
    )
    .unwrap();
    assert_eq!(
        audio,
        render(
            bundle.project.timebase,
            chime,
            &RenderLayout::Full { loop_passes: 1 },
            None,
            &reference_soundfont(),
            RATE
        )
        .unwrap()
    );
    assert_eq!(audio.content_samples, 22_050);
    assert!(!audio.tail_truncated);

    // Key 84 is 1046.5 Hz; +4096 of a ±2 semitone range raises it one semitone.
    let unbent = frequency(&audio, 0.185, 0.235);
    let bent = frequency(&audio, 0.305, 0.325);
    assert!((unbent / 1046.5 - 1.0).abs() < 0.02, "{unbent}");
    assert!((bent / 1108.7 - 1.0).abs() < 0.02, "{bent}");

    // Expression drops from 110 to 25 before the note is released at 0.42 s.
    assert!(rms(&audio, 0.405, 0.415) < rms(&audio, 0.305, 0.325) * 0.25);
    assert!(rms(&audio, 0.43, 0.44) > 0.0, "release is audible");
    assert!(
        rms(&audio, 0.49, 0.50) < 1e-4,
        "silent before the content ends"
    );
    assert!(
        audio.left.len() as u64 > audio.content_samples,
        "the tail keeps the final silent block"
    );
}

#[test]
fn second_loop_pass_starts_at_the_mapped_sample() {
    let bundle = common::village();
    let bgm = &bundle.revisions[0].composition;
    let melody = ["melody".to_owned()];
    let audio = render(
        bundle.project.timebase,
        bgm,
        &RenderLayout::Full { loop_passes: 2 },
        Some(&melody),
        &reference_soundfont(),
        RATE,
    )
    .unwrap();
    assert_eq!(
        audio.content_samples,
        sample_at(Rational::new(124, 5), RATE)
    );

    // The melody rests from 14.0 s until the loop returns at 14.4 s.
    let second_pass = sample_at(Rational::new(72, 5), RATE) as f64 / RATE as f64;
    assert!(rms(&audio, second_pass - 0.05, second_pass) < 1e-4);
    assert!(rms(&audio, second_pass, second_pass + 0.05) > 0.05);
    // The first pass enters at the same source position, 4 s.
    assert!(rms(&audio, 3.95, 4.0) < 1e-4 && rms(&audio, 4.0, 4.05) > 0.05);
}

#[test]
fn excerpts_render_only_their_range() {
    let bundle = common::village();
    let bgm = &bundle.revisions[1].composition;
    let layout = RenderLayout::Excerpt {
        start: 5760,
        end: 7680,
    };
    let audio = render(
        bundle.project.timebase,
        bgm,
        &layout,
        Some(&["accompaniment".to_owned()]),
        &reference_soundfont(),
        RATE,
    )
    .unwrap();
    assert_eq!(audio.content_samples, 2 * RATE as u64);
    // Bar 4 accompaniment after the edit starts on A3, 220 Hz.
    assert!((frequency(&audio, 0.05, 0.45) / 220.0 - 1.0).abs() < 0.02);
    let wav = wav_bytes(&audio);
    assert_eq!(&wav[..4], b"RIFF");
    assert_eq!(wav.len(), 44 + audio.left.len() * 4);
}

#[test]
fn loop_pass_two_repeats_pass_one_and_notes_start_on_their_sample() {
    let bundle = common::village();
    let bgm = &bundle.revisions[0].composition;
    let melody = ["melody".to_owned()];
    let audio = render(
        bundle.project.timebase,
        bgm,
        &RenderLayout::Full { loop_passes: 2 },
        Some(&melody),
        &reference_soundfont(),
        RATE,
    )
    .unwrap();
    let pass_one = sample_at(Rational::from_integer(4), RATE) as usize;
    let pass_two = sample_at(Rational::new(72, 5), RATE) as usize;
    let length = pass_two - pass_one;
    // The melody enters after silence exactly on its scheduled sample.
    for start in [pass_one, pass_two] {
        assert!(audio.left[start - 2000..start].iter().all(|s| *s == 0.0));
        assert_ne!(audio.left[start], 0.0, "onset at sample {start}");
    }
    // The loop region plays the same samples in both passes.
    assert_eq!(
        audio.left[pass_one..pass_two],
        audio.left[pass_two..pass_two + length]
    );
    assert_eq!(
        audio.right[pass_one..pass_two],
        audio.right[pass_two..pass_two + length]
    );
}

#[test]
fn reverb_sends_are_heard_at_any_level() {
    use tunemend::composition::{ControlKind, ControlLane, ControlPoint};
    let bundle = common::chime();
    let mut quiet = bundle.revisions[0].composition.clone();
    for note in &mut quiet.notes {
        note.velocity = 40;
    }
    let with_send = |send: Option<i32>| {
        let mut composition = quiet.clone();
        if let Some(value) = send {
            composition.control_lanes.push(ControlLane {
                voice: "lead".into(),
                kind: ControlKind::Reverb,
                points: vec![ControlPoint { at: 0, value }],
            });
        }
        render(
            bundle.project.timebase,
            &composition,
            &RenderLayout::Full { loop_passes: 1 },
            None,
            &reference_soundfont(),
            RATE,
        )
        .unwrap()
    };
    let dry = with_send(None);
    assert_eq!(with_send(Some(0)), dry, "a zero send changes nothing");
    for send in [20, 50, 64] {
        let wet = with_send(Some(send));
        let shared = dry.left.len().min(wet.left.len());
        assert_ne!(wet.left[..shared], dry.left[..shared], "CC91 {send}");
        assert!(
            wet.left.len() > dry.left.len(),
            "CC91 {send} adds a reverb tail"
        );
    }
}
