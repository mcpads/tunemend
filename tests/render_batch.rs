//! render_batch stems, regions and WAV formats, and markers and sections as
//! timeline data: sums, crops, decoding, refusals, regeneration and MIDI.

mod common;

use serde_json::{Value, json};
use tunemend::service::{Service, Status, ToolResponse};

struct Workspace {
    path: std::path::PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "tunemend-test-render-batch-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn ok(service: &mut Service, tool: &str, args: Value) -> ToolResponse {
    let response = service.call(tool, args);
    assert_eq!(response.status, Status::Ok, "{tool}: {response:#?}");
    response
}

/// The code and location of a refused call.
fn refused(service: &mut Service, tool: &str, args: Value) -> (String, String) {
    let response = service.call(tool, args);
    assert_eq!(response.status, Status::Blocked, "{tool}: {response:#?}");
    let d = &response.diagnostics[0];
    (
        serde_json::to_value(d.code)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned(),
        d.location.clone(),
    )
}

fn village(service: &mut Service) {
    let bundle = common::village();
    ok(
        service,
        "create_project",
        json!({"job_id": "create", "project": "village", "timebase": bundle.project.timebase,
            "composition": bundle.revisions[0].composition}),
    );
}

/// r2: the band bus (EQ, sidechained compressor, note delay) fed by the
/// accompaniment and a melody send, melody pan automation, a master gain,
/// reverb and limiter. Every shared stage is linear or gain-based.
fn mixed_village(service: &mut Service) {
    village(service);
    ok(
        service,
        "edit_project",
        json!({"project": "village", "request": {"job_id": "mix", "base_revision": "r1",
        "scope": {"buses": true}, "operations": [
            {"op": "add_bus", "bus": {"id": "band"}},
            {"op": "set_mix", "target": {"kind": "voice", "id": "accompaniment"}, "volume_db": -3.0, "pan": -30.0, "output": "band"},
            {"op": "set_send", "send": {"voice": "melody", "bus": "band", "gain_db": -6.0}},
            {"op": "set_mix", "target": {"kind": "master"}, "volume_db": -1.5},
            {"op": "set_mix_automation", "target": {"kind": "voice", "id": "melody"}, "parameter": "pan", "points": [
                {"at": 0, "value": -20.0, "shape": "linear"}, {"at": 3840, "value": 20.0}]},
            {"op": "set_effects", "target": {"kind": "bus", "id": "band"}, "effects": [
                {"id": "eq", "processor": {"kind": "eq", "bands": [{"filter": "peaking", "frequency_hz": 1000.0, "gain_db": 3.0, "q": 1.0}]}},
                {"id": "comp", "processor": {"kind": "compressor", "threshold_db": -30.0, "ratio": 4.0, "attack_ms": 5.0, "release_ms": 80.0, "sidechain": {"kind": "voice", "id": "bass"}}},
                {"id": "dly", "processor": {"kind": "delay", "time": {"unit": "note", "numerator": 1, "denominator": 8}, "feedback_percent": 30.0, "mix_percent": 20.0}}]},
            {"op": "set_effects", "target": {"kind": "master"}, "effects": [
                {"id": "verb", "processor": {"kind": "reverb", "room_size_percent": 50.0, "damping_percent": 50.0, "mix_percent": 15.0}},
                {"id": "lim", "processor": {"kind": "limiter", "ceiling_db": -12.0, "release_ms": 50.0, "lookahead_ms": 5.0}}]}
        ]}}),
    );
}

/// Interleaved samples of a WAV as f64, with its bit depth and format tag.
fn decode(path: &str) -> (u16, u16, Vec<f64>) {
    let bytes = std::fs::read(path).unwrap();
    let wav = tunemend::wav::parse(&bytes).unwrap();
    let width = usize::from(wav.bits / 8);
    let samples = wav
        .data
        .chunks(width)
        .map(|s| tunemend::wav::normalized(wav.format, s))
        .collect();
    (wav.bits, wav.format_tag, samples)
}

/// Integer codes of a 16- or 24-bit PCM WAV.
fn codes(path: &str) -> Vec<i32> {
    let bytes = std::fs::read(path).unwrap();
    let wav = tunemend::wav::parse(&bytes).unwrap();
    match wav.bits {
        16 => wav
            .data
            .chunks(2)
            .map(|s| i32::from(i16::from_le_bytes([s[0], s[1]])))
            .collect(),
        24 => wav
            .data
            .chunks(3)
            .map(|s| i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8)
            .collect(),
        bits => panic!("{bits}-bit"),
    }
}

/// The f32 samples of a float WAV, exactly.
fn floats(path: &str) -> Vec<f32> {
    let (bits, tag, samples) = decode(path);
    assert_eq!((bits, tag), (32, 3));
    samples.into_iter().map(|s| s as f32).collect()
}

fn renders(response: &ToolResponse) -> Vec<Value> {
    response.result["renders"].as_array().unwrap().clone()
}

fn path(render: &Value) -> String {
    render["path"].as_str().unwrap().to_owned()
}

#[test]
fn voice_stems_sum_to_the_mixdown_through_bus_and_master_dynamics() {
    let space = Workspace::new("voice-stems");
    let mut s = Service::new(&space.path);
    mixed_village(&mut s);
    let batch = ok(
        &mut s,
        "render_batch",
        json!({"job_id": "stems", "project": "village", "stems": {"by": "voice"}, "format": "float32"}),
    );
    let listed = renders(&batch);
    assert_eq!(listed.len(), 4, "the mixdown and three voice stems");
    assert_eq!(listed[0]["render"]["part"], json!({"kind": "mixdown"}));
    assert_eq!(
        listed[1]["render"]["part"],
        json!({"kind": "voice", "id": "melody"})
    );
    assert_eq!(listed[1]["render"]["format"], "float32");
    let check = &batch.result["spans"][0]["sum_check"];
    assert_eq!(check["sums_within_rounding"], true, "{check}");
    assert_eq!(check["covers_mix"], true);
    let mixdown = floats(&path(&listed[0]));
    let stems: Vec<Vec<f32>> = listed[1..].iter().map(|r| floats(&path(r))).collect();
    let mut largest = 0.0f64;
    for (n, value) in mixdown.iter().enumerate() {
        let sum: f64 = stems.iter().map(|s| f64::from(s[n])).sum();
        largest = largest.max((sum - f64::from(*value)).abs());
    }
    assert_eq!(largest, check["max_abs_difference"].as_f64().unwrap());
    let peak = check["reference_peak"].as_f64().unwrap();
    assert!(peak > 0.1, "a real mix: peak {peak}");
    assert!(largest < 1e-6, "stems sum within f32 rounding: {largest}");

    // The limiter reduced the mix, and every stem carries that reduction.
    let ceiling = 10f64.powf(-12.0 / 20.0);
    assert!(peak <= ceiling + 1e-6);
    let preview = ok(
        &mut s,
        "render_preview",
        json!({"job_id": "preview", "project": "village", "layout": {"kind": "full", "loop_passes": 1}}),
    );
    let mixdown16 = ok(
        &mut s,
        "render_batch",
        json!({"job_id": "stems16", "project": "village", "stems": {"by": "voice", "voices": ["bass"]}}),
    );
    assert_eq!(
        renders(&mixdown16)[0]["render"]["sha256"],
        preview.result["render"]["sha256"],
        "with a mixer the batch mixdown is render_preview's render"
    );
    let partial = &mixdown16.result["spans"][0]["sum_check"];
    assert_eq!(partial["covers_mix"], false);
    assert_eq!(
        partial["uncovered_voices"],
        json!(["melody", "accompaniment"])
    );
}

#[test]
fn stems_of_voices_on_a_neutral_master_add_up_bit_exactly() {
    let space = Workspace::new("exact");
    let mut s = Service::new(&space.path);
    village(&mut s);
    let batch = ok(
        &mut s,
        "render_batch",
        json!({"job_id": "stems", "project": "village", "stems": {"by": "voice"}, "format": "float32",
            "spans": [{"kind": "excerpt", "start": 0, "end": 7680}]}),
    );
    let listed = renders(&batch);
    let mixdown = floats(&path(&listed[0]));
    let stems: Vec<Vec<f32>> = listed[1..].iter().map(|r| floats(&path(r))).collect();
    // The mixer adds voices in order in f32; so does this sum.
    for (n, value) in mixdown.iter().enumerate() {
        let sum = stems
            .iter()
            .fold(0.0f32, |sum, s| (f64::from(sum) + f64::from(s[n])) as f32);
        assert_eq!(sum.to_bits(), value.to_bits(), "sample {n}");
    }
    // The reported check sums in f64, so it shows only the f32 rounding.
    let reported = batch.result["spans"][0]["sum_check"]["max_abs_difference"]
        .as_f64()
        .unwrap();
    assert!(reported < 1e-7, "{reported}");
}

#[test]
fn bus_and_group_stems_complete_the_mix_and_pre_master_stems_skip_it() {
    let space = Workspace::new("bus-stems");
    let mut s = Service::new(&space.path);
    mixed_village(&mut s);
    let buses = ok(
        &mut s,
        "render_batch",
        json!({"job_id": "buses", "project": "village", "stems": {"by": "bus"}, "mixdown": false, "format": "float32"}),
    );
    let parts: Vec<Value> = renders(&buses)
        .iter()
        .map(|r| r["render"]["part"].clone())
        .collect();
    assert_eq!(
        parts,
        vec![
            json!({"kind": "bus", "id": "band"}),
            json!({"kind": "direct"})
        ]
    );
    let check = &buses.result["spans"][0]["sum_check"];
    assert_eq!(check["sums_within_rounding"], true);
    assert!(check["max_abs_difference"].as_f64().unwrap() < 1e-6);

    let groups = ok(
        &mut s,
        "render_batch",
        json!({"job_id": "groups", "project": "village", "format": "float32",
            "stems": {"by": "group", "groups": [{"id": "lead", "voices": ["melody"]}, {"id": "rhythm", "voices": ["accompaniment", "bass"]}]}}),
    );
    assert_eq!(
        renders(&groups)[2]["render"]["part"],
        json!({"kind": "group", "id": "rhythm", "voices": ["accompaniment", "bass"]})
    );
    assert!(
        groups.result["spans"][0]["sum_check"]["max_abs_difference"]
            .as_f64()
            .unwrap()
            < 1e-6
    );

    let pre = ok(
        &mut s,
        "render_batch",
        json!({"job_id": "pre", "project": "village", "stems": {"by": "voice"}, "stem_master": "pre_master", "format": "float32"}),
    );
    let listed = renders(&pre);
    assert_eq!(listed[0]["render"].get("pre_master"), None);
    assert_eq!(listed[1]["render"]["pre_master"], true);
    let check = &pre.result["spans"][0]["sum_check"];
    assert!(
        check["reference"]
            .as_str()
            .unwrap()
            .starts_with("the master input")
    );
    assert!(check["max_abs_difference"].as_f64().unwrap() < 1e-6);
    // Before the -12 dBFS limiter the stems sum louder than the mixdown.
    assert!(check["reference_peak"].as_f64().unwrap() > 10f64.powf(-12.0 / 20.0) + 0.01);
}

#[test]
fn a_shared_snes_echo_is_reported_as_approximate() {
    let space = Workspace::new("echo");
    let mut s = Service::new(&space.path);
    village(&mut s);
    ok(
        &mut s,
        "edit_project",
        json!({"project": "village", "request": {"job_id": "echo", "base_revision": "r1",
        "scope": {"buses": true}, "operations": [
            {"op": "set_effects", "target": {"kind": "master"}, "effects": [
                {"id": "echo", "processor": {"kind": "snes_echo", "fir": [127, 0, 0, 0, 0, 0, 0, 0], "delay": 2, "feedback": 60, "volume_left": 50, "volume_right": 50}}]}]}}),
    );
    let batch = ok(
        &mut s,
        "render_batch",
        json!({"job_id": "stems", "project": "village", "stems": {"by": "voice"}, "mixdown": false,
            "spans": [{"kind": "excerpt", "start": 0, "end": 3840}]}),
    );
    let check = &batch.result["spans"][0]["sum_check"];
    assert_eq!(check["sums_within_rounding"], false);
    assert_eq!(
        check["approximated_by"],
        json!(["effect `echo` on the master"])
    );
    let difference = check["max_abs_difference"].as_f64().unwrap();
    assert!(
        difference > 0.0 && difference < 0.05,
        "echo shares differ from the shared echo by quantization only: {difference}"
    );
    // Pre-master stems avoid the echo and sum within rounding.
    let pre = ok(
        &mut s,
        "render_batch",
        json!({"job_id": "pre", "project": "village", "stems": {"by": "voice"}, "mixdown": false, "stem_master": "pre_master",
            "spans": [{"kind": "excerpt", "start": 0, "end": 3840}]}),
    );
    assert_eq!(
        pre.result["spans"][0]["sum_check"]["sums_within_rounding"],
        true
    );
}

#[test]
fn region_renders_equal_their_excerpts_and_source_backed_crops() {
    let space = Workspace::new("regions");
    let mut s = Service::new(&space.path);
    mixed_village(&mut s);
    let batch = ok(
        &mut s,
        "render_batch",
        json!({"job_id": "regions", "project": "village", "spans": [{"kind": "sections"}, {"kind": "full", "loop_passes": 2}]}),
    );
    let spans = batch.result["spans"].as_array().unwrap();
    assert_eq!(spans.len(), 4);
    let regions: Vec<&str> = spans[..3]
        .iter()
        .map(|s| s["region"].as_str().unwrap())
        .collect();
    assert_eq!(regions, ["intro", "theme-a", "theme-b"]);
    assert_eq!(spans[3]["region"], Value::Null);
    for (span, (start, end)) in spans.iter().zip([(0, 3840), (3840, 9600), (9600, 15360)]) {
        let preview = ok(
            &mut s,
            "render_preview",
            json!({"job_id": format!("excerpt-{start}"), "project": "village", "layout": {"kind": "excerpt", "start": start, "end": end}}),
        );
        let id = span["renders"][0].as_str().unwrap();
        let record = renders(&batch)
            .into_iter()
            .find(|r| r["render"]["id"] == id)
            .unwrap();
        assert_eq!(
            record["render"]["sha256"], preview.result["render"]["sha256"],
            "{id}"
        );
        assert_eq!(record["render"]["region"], span["region"]);
        assert_eq!(record["render"].get("part"), None);
    }
    let one = ok(
        &mut s,
        "render_batch",
        json!({"job_id": "one", "project": "village", "spans": [{"kind": "section", "id": "theme-a"}]}),
    );
    assert_eq!(
        renders(&one)[0]["render"]["sha256"],
        renders(&batch)[1]["render"]["sha256"]
    );

    // A source-backed region is the full render's PCM over that region.
    ok(
        &mut s,
        "export_project",
        json!({"job_id": "export", "project": "village", "revision": "r1", "destination": "out/v", "render_audio": "none"}),
    );
    ok(
        &mut s,
        "import_midi",
        json!({"job_id": "import", "project": "src", "path": "out/v/village.mid"}),
    );
    ok(
        &mut s,
        "edit_project",
        json!({"project": "src", "request": {"job_id": "sections", "base_revision": "r1", "scope": {"timeline": true},
            "operations": [{"op": "set_sections", "sections": [{"id": "middle", "start": 3840, "end": 9600}]}]}}),
    );
    let full = ok(
        &mut s,
        "render_batch",
        json!({"job_id": "src-full", "project": "src", "format": "pcm24"}),
    );
    let region = ok(
        &mut s,
        "render_batch",
        json!({"job_id": "src-region", "project": "src", "format": "pcm24", "spans": [{"kind": "section", "id": "middle"}]}),
    );
    let content = |r: &ToolResponse| {
        (r.result["spans"][0]["timing"]["content_seconds"]
            .as_f64()
            .unwrap()
            * 44_100.0)
            .round() as usize
    };
    let whole = codes(&path(&renders(&full)[0]));
    let part = codes(&path(&renders(&region)[0]));
    let start_seconds = 3840.0 / 480.0 * 60.0 / 120.0;
    let tempo: Value =
        serde_json::to_value(&common::village().revisions[0].composition.tempo_map).unwrap();
    assert_eq!(tempo[0]["microseconds_per_quarter"], 500_000, "120 BPM");
    let crop = (start_seconds * 44_100.0) as usize * 2;
    let frames = content(&region) * 2;
    assert!(frames > 0 && part.len() >= frames);
    assert_eq!(&part[..frames], &whole[crop..crop + frames]);
}

#[test]
fn levels_beyond_full_scale_are_reported_apart_from_the_encoding_clamp() {
    let space = Workspace::new("levels");
    let mut s = Service::new(&space.path);
    village(&mut s);
    // Unity gain stays below full scale: levels, no warning.
    let quiet = ok(
        &mut s,
        "render_batch",
        json!({"job_id": "quiet", "project": "village", "format": "pcm24"}),
    );
    let levels = &renders(&quiet)[0]["levels"];
    assert_eq!(levels["samples_over_full_scale"], 0, "{levels}");
    assert_eq!(levels["clamped_samples"], 0);
    assert!(levels["sample_peak"].as_f64().unwrap() < 1.0);
    assert!(quiet.result.get("warnings").is_none());
    let preview = ok(
        &mut s,
        "render_preview",
        json!({"job_id": "quiet-preview", "project": "village", "layout": {"kind": "full", "loop_passes": 1}}),
    );
    assert_eq!(preview.result["levels"]["clamped_samples"], 0);
    assert!(preview.result.get("warnings").is_none());

    ok(
        &mut s,
        "edit_project",
        json!({"project": "village", "request": {"job_id": "loud", "base_revision": "r1",
            "scope": {"voices": [], "buses": true},
            "operations": [{"op": "set_mix", "target": {"kind": "master"}, "volume_db": 12.0}]}}),
    );
    let render = |s: &mut Service, job: &str, format: &str| {
        let response = ok(
            s,
            "render_batch",
            json!({"job_id": job, "project": "village", "format": format}),
        );
        (renders(&response)[0].clone(), response)
    };
    let (float, float_response) = render(&mut s, "loud-float", "float32");
    let (pcm16, pcm16_response) = render(&mut s, "loud-pcm16", "pcm16");
    let (pcm24, _) = render(&mut s, "loud-pcm24", "pcm24");
    let samples = floats(&path(&float));
    let over = samples.iter().filter(|x| x.abs() > 1.0).count();
    let peak = samples.iter().fold(0.0f32, |p, x| p.max(x.abs()));
    assert!(over > 0, "the +12 dB master must exceed full scale");

    // Float keeps the levels: counted, not clamped, and warned about.
    assert_eq!(float["levels"]["samples_over_full_scale"], over);
    assert_eq!(float["levels"]["clamped_samples"], 0);
    assert_eq!(
        float["levels"]["sample_peak"].as_f64().unwrap(),
        f64::from(peak)
    );
    let warning = &float_response.result["warnings"][0];
    assert_eq!(warning["code"], "samples_over_full_scale");
    assert_eq!(
        warning["location"],
        "/renders/0/levels/samples_over_full_scale"
    );
    assert!(
        samples.iter().any(|x| x.abs() > 1.0),
        "float is not clamped"
    );

    // Integer files clamp exactly those samples; the bytes are the
    // documented conversion of the same float samples, nothing rescaled.
    for (record, full_scale) in [(&pcm16, 32767), (&pcm24, 8_388_607)] {
        assert_eq!(record["levels"]["clamped_samples"], over, "{record}");
        assert_eq!(record["levels"]["samples_over_full_scale"], over);
        let file = codes(&path(record));
        let at_full_scale = file.iter().filter(|c| c.abs() >= full_scale).count();
        assert!(at_full_scale >= over, "{at_full_scale} < {over}");
    }
    let expected16: Vec<i32> = samples
        .iter()
        .map(|x| i32::from((x.clamp(-1.0, 1.0) * 32767.0).round() as i16))
        .collect();
    assert_eq!(codes(&path(&pcm16)), expected16);
    let warning = &pcm16_response.result["warnings"][0];
    assert_eq!(warning["code"], "integer_clipping");
    assert_eq!(warning["location"], "/renders/0/levels/clamped_samples");
    assert!(
        pcm16_response.summary.contains("exceed full scale"),
        "{}",
        pcm16_response.summary
    );

    // render_preview writes the same 16-bit bytes and reports the clamp.
    let preview = ok(
        &mut s,
        "render_preview",
        json!({"job_id": "loud-preview", "project": "village", "layout": {"kind": "full", "loop_passes": 1}}),
    );
    assert_eq!(
        preview.result["render"]["sha256"],
        pcm16["render"]["sha256"]
    );
    assert_eq!(preview.result["levels"]["clamped_samples"], over);
    assert_eq!(preview.result["warnings"][0]["code"], "integer_clipping");
    assert_eq!(
        preview.result["warnings"][0]["location"],
        "/levels/clamped_samples"
    );
}

#[test]
fn full_spans_of_a_looping_revision_report_intro_and_loop_lengths() {
    let space = Workspace::new("loop-timing");
    let mut s = Service::new(&space.path);
    village(&mut s);
    let batch = ok(
        &mut s,
        "render_batch",
        json!({"job_id": "passes", "project": "village", "spans": [
            {"kind": "full", "loop_passes": 2}, {"kind": "section", "id": "intro"}]}),
    );
    let preview = ok(
        &mut s,
        "render_preview",
        json!({"job_id": "passes-preview", "project": "village", "layout": {"kind": "full", "loop_passes": 2}}),
    );
    let timing = &batch.result["spans"][0]["timing"];
    for key in ["intro_seconds", "loop_seconds", "loop_passes"] {
        assert_eq!(timing[key], preview.result["timing"][key], "{key}");
    }
    let content = timing["content_seconds"].as_f64().unwrap();
    let parts =
        timing["intro_seconds"].as_f64().unwrap() + 2.0 * timing["loop_seconds"].as_f64().unwrap();
    assert!((content - parts).abs() < 1e-9, "{content} vs {parts}");
    assert!(
        batch.result["spans"][1]["timing"]
            .get("loop_seconds")
            .is_none()
    );
}

#[test]
fn formats_decode_to_the_renderers_samples() {
    let space = Workspace::new("formats");
    let mut s = Service::new(&space.path);
    mixed_village(&mut s);
    let span = json!([{"kind": "section", "id": "intro"}]);
    let render = |s: &mut Service, job: &str, format: &str| {
        let response = ok(
            s,
            "render_batch",
            json!({"job_id": job, "project": "village", "spans": span, "format": format, "sample_rate": 48000}),
        );
        renders(&response)[0].clone()
    };
    let float = render(&mut s, "float", "float32");
    let pcm24 = render(&mut s, "pcm24", "pcm24");
    let pcm16 = render(&mut s, "pcm16", "pcm16");
    assert_eq!(float["render"]["sample_rate"], 48000);
    assert_eq!(pcm16["render"].get("format"), None, "16-bit is the default");
    let samples = floats(&path(&float));
    let (bits, tag, _) = decode(&path(&pcm24));
    assert_eq!((bits, tag), (24, 1));
    let expected24: Vec<i32> = samples
        .iter()
        .map(|x| (f64::from(x.clamp(-1.0, 1.0)) * 8_388_607.0).round() as i32)
        .collect();
    assert_eq!(codes(&path(&pcm24)), expected24);
    let expected16: Vec<i32> = samples
        .iter()
        .map(|x| i32::from((x.clamp(-1.0, 1.0) * 32767.0).round() as i16))
        .collect();
    assert_eq!(codes(&path(&pcm16)), expected16);
    assert_eq!(
        float["render"]["length_samples"],
        pcm24["render"]["length_samples"]
    );
    // Measurement reads every format and finds the same peak.
    let peak = samples
        .iter()
        .fold(0.0f64, |p, x| p.max(f64::from(x.abs())));
    for record in [&float, &pcm24] {
        let measured = ok(
            &mut s,
            "measure_audio",
            json!({"audio": {"project": "village", "render": record["render"]["id"]}}),
        );
        let dbfs = measured.result["levels"]["sample_peak_dbfs"]
            .as_f64()
            .unwrap();
        assert!(
            (dbfs - 20.0 * peak.log10()).abs() <= 0.005,
            "{dbfs}, reported to 0.01 dB"
        );
    }
}

#[test]
fn unknown_regions_voices_and_oversized_batches_are_refused() {
    let space = Workspace::new("refusals");
    let mut s = Service::new(&space.path);
    mixed_village(&mut s);
    let call = |s: &mut Service, extra: Value| {
        let mut args = json!({"job_id": format!("job-{}", extra), "project": "village"});
        for (k, v) in extra.as_object().unwrap() {
            args[k] = v.clone();
        }
        refused(s, "render_batch", args)
    };
    let pair = |code: &str, at: &str| (code.to_owned(), at.to_owned());
    assert_eq!(
        call(
            &mut s,
            json!({"spans": [{"kind": "full", "loop_passes": 1}, {"kind": "section", "id": "chorus"}]})
        ),
        pair("unknown_reference", "/spans/1/id")
    );
    assert_eq!(
        call(
            &mut s,
            json!({"stems": {"by": "voice", "voices": ["drums"]}})
        ),
        pair("unknown_reference", "/stems/voices/0")
    );
    assert_eq!(
        call(
            &mut s,
            json!({"stems": {"by": "group", "groups": [{"id": "a", "voices": ["melody"]}, {"id": "b", "voices": ["bass", "drums"]}]}})
        ),
        pair("unknown_reference", "/stems/groups/1/voices/1")
    );
    assert_eq!(
        call(
            &mut s,
            json!({"stems": {"by": "group", "groups": [{"id": "a", "voices": ["melody"]}, {"id": "b", "voices": ["bass", "melody"]}]}})
        ),
        pair("duplicate_id", "/stems/groups/1/voices/1")
    );
    assert_eq!(
        call(&mut s, json!({"mixdown": false})),
        pair("invalid_range", "/stems")
    );
    assert_eq!(
        call(
            &mut s,
            json!({"spans": [{"kind": "excerpt", "start": 9000, "end": 3000}]})
        ),
        pair("invalid_render_layout", "/spans/0")
    );
    assert_eq!(
        call(&mut s, json!({"sample_rate": 8000})),
        pair("value_out_of_range", "/sample_rate")
    );
    let many: Vec<Value> = (0..17)
        .map(|_| json!({"kind": "section", "id": "intro"}))
        .collect();
    assert_eq!(
        call(&mut s, json!({"spans": many, "stems": {"by": "voice"}})),
        pair("value_out_of_range", "/spans")
    );
    let chime = common::chime();
    ok(
        &mut s,
        "create_project",
        json!({"job_id": "chime", "project": "chime", "timebase": chime.project.timebase,
            "composition": chime.revisions[0].composition}),
    );
    assert_eq!(
        refused(
            &mut s,
            "render_batch",
            json!({"job_id": "no-sections", "project": "chime", "spans": [{"kind": "sections"}]})
        ),
        pair("unknown_reference", "/spans/0")
    );
    // Nothing was stored by the refusals.
    let renders = s.call(
        "inspect_project",
        json!({"view": "reviews", "project": "village"}),
    );
    assert!(
        !renders.result.to_string().contains("render-1"),
        "{}",
        renders.result
    );
}

#[test]
fn batch_renders_present_export_and_regenerate_by_hash() {
    let space = Workspace::new("delivery");
    let mut s = Service::new(&space.path);
    mixed_village(&mut s);
    let args = json!({"job_id": "stems", "project": "village", "stems": {"by": "bus"},
        "spans": [{"kind": "section", "id": "theme-a"}], "format": "float32", "sample_rate": 32000});
    let batch = ok(&mut s, "render_batch", args.clone());
    let again = ok(&mut s, "render_batch", args);
    assert!(again.replayed);
    let ids: Vec<String> = renders(&batch)
        .iter()
        .map(|r| r["render"]["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, ["render-1", "render-2", "render-3"]);
    ok(
        &mut s,
        "present_review",
        json!({"job_id": "present", "project": "village", "renders": [
            {"render": "render-1", "label": "Mix"}, {"render": "render-2", "label": "Band bus"}]}),
    );
    ok(
        &mut s,
        "export_project",
        json!({"job_id": "export", "project": "village", "destination": "out/stems", "render_audio": "listed", "render_ids": ["render-2"]}),
    );
    let folder = space.path.join("out/stems");
    assert!(folder.join("renders/render-2.wav").is_file());
    assert!(!folder.join("renders/render-3.wav").exists());

    let other = Workspace::new("delivery-import");
    std::fs::create_dir_all(other.path.join("in")).unwrap();
    std::fs::copy(
        folder.join("village.project.json"),
        other.path.join("in/village.project.json"),
    )
    .unwrap();
    let mut t = Service::new(&other.path);
    let imported = ok(
        &mut t,
        "import_project",
        json!({"job_id": "import", "path": "in/village.project.json", "missing_renders": "regenerate"}),
    );
    let checks = imported.result["regenerated_checks"].as_array().unwrap();
    assert_eq!(checks.len(), 3);
    assert!(
        checks
            .iter()
            .all(|c| c["checked"] == "sha256 equals the recorded render")
    );
}

#[test]
fn named_copies_give_delivered_mixes_and_stems_readable_names() {
    let space = Workspace::new("named");
    let mut s = Service::new(&space.path);
    mixed_village(&mut s);
    // render-1..3: theme-a mix and two bus stems; render-4..7: the whole
    // piece's mix and three voice stems; render-8: a melody-only preview.
    ok(
        &mut s,
        "render_batch",
        json!({"job_id": "bus", "project": "village", "stems": {"by": "bus"},
            "spans": [{"kind": "section", "id": "theme-a"}]}),
    );
    ok(
        &mut s,
        "render_batch",
        json!({"job_id": "voice", "project": "village", "stems": {"by": "voice"}}),
    );
    ok(
        &mut s,
        "render_preview",
        json!({"job_id": "solo", "project": "village", "layout": {"kind": "full", "loop_passes": 1}, "voices": ["melody"]}),
    );
    let plain = ok(
        &mut s,
        "export_project",
        json!({"job_id": "plain", "project": "village", "destination": "out/plain"}),
    );
    assert!(plain.result.get("named_copies").is_none());
    assert!(!space.path.join("out/plain/mix").exists());
    let named = ok(
        &mut s,
        "export_project",
        json!({"job_id": "named", "project": "village", "destination": "out/named", "named_copies": true}),
    );
    let listed: Vec<(std::path::PathBuf, String)> = named.result["named_copies"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                std::path::PathBuf::from(f["path"].as_str().unwrap()),
                f["render"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let expected = [
        ("mix/theme-a.wav", "render-1"),
        ("stems/theme-a/bus-band.wav", "render-2"),
        ("stems/theme-a/direct.wav", "render-3"),
        ("mix/full.wav", "render-4"),
        ("stems/full/voice-melody.wav", "render-5"),
        ("stems/full/voice-accompaniment.wav", "render-6"),
        ("stems/full/voice-bass.wav", "render-7"),
        ("mix/full-only-melody.wav", "render-8"),
    ];
    let mut sorted = listed.clone();
    sorted.sort();
    let mut wanted: Vec<(std::path::PathBuf, String)> = expected
        .iter()
        .map(|(p, r)| (std::path::PathBuf::from(p), (*r).to_owned()))
        .collect();
    wanted.sort();
    assert_eq!(sorted, wanted);
    let folder = space.path.join("out/named");
    for (path, render) in &listed {
        assert_eq!(
            std::fs::read(folder.join(path)).unwrap(),
            std::fs::read(folder.join(format!("renders/{render}.wav"))).unwrap(),
            "{}",
            path.display()
        );
    }
    let report: Value =
        serde_json::from_slice(&std::fs::read(folder.join("report.json")).unwrap()).unwrap();
    let copy = report["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| {
            std::path::Path::new(f["path"].as_str().unwrap())
                == std::path::Path::new("stems/full/voice-bass.wav")
        })
        .unwrap();
    assert_eq!(copy["kind"], "wav_named_copy");
    assert_eq!(
        std::path::Path::new(copy["copy_of"].as_str().unwrap()),
        std::path::Path::new("renders/render-7.wav")
    );
    assert_eq!(copy["part"], json!({"kind": "voice", "id": "bass"}));
    // Import reads only the bundle and renders/<id>.wav.
    let other = Workspace::new("named-import");
    std::fs::create_dir_all(other.path.join("in/renders")).unwrap();
    std::fs::copy(
        folder.join("village.project.json"),
        other.path.join("in/village.project.json"),
    )
    .unwrap();
    for (_, render) in &listed {
        std::fs::copy(
            folder.join(format!("renders/{render}.wav")),
            other.path.join(format!("in/renders/{render}.wav")),
        )
        .unwrap();
    }
    let mut t = Service::new(&other.path);
    ok(
        &mut t,
        "import_project",
        json!({"job_id": "import", "path": "in/village.project.json"}),
    );
}

#[test]
fn markers_are_timeline_data_and_sections_can_be_written_as_midi_markers() {
    let space = Workspace::new("markers");
    let mut s = Service::new(&space.path);
    village(&mut s);
    let markers = json!([{"at": 0, "text": "start"}, {"at": 3840, "text": "loop"}, {"at": 9600, "kind": "cue_point", "text": "drums in"}]);
    let out_of_scope = refused(
        &mut s,
        "edit_project",
        json!({"project": "village", "request": {"job_id": "markers-scope", "base_revision": "r1",
            "operations": [{"op": "set_markers", "markers": markers}]}}),
    );
    assert_eq!(
        out_of_scope,
        ("out_of_scope".into(), "/scope/timeline".into())
    );
    let unordered = refused(
        &mut s,
        "edit_project",
        json!({"project": "village", "request": {"job_id": "markers-order", "base_revision": "r1", "scope": {"timeline": true},
            "operations": [{"op": "set_markers", "markers": [{"at": 3840, "text": "b"}, {"at": 0, "text": "a"}]}]}}),
    );
    assert_eq!(unordered.0, "control_points_unordered");
    ok(
        &mut s,
        "edit_project",
        json!({"project": "village", "request": {"job_id": "markers", "base_revision": "r1", "scope": {"timeline": true},
            "operations": [{"op": "set_markers", "markers": markers}]}}),
    );
    let summary = ok(
        &mut s,
        "inspect_project",
        json!({"view": "summary", "project": "village"}),
    );
    assert_eq!(
        summary.result["markers"][0],
        json!({"at": 0, "kind": "marker", "text": "start"})
    );
    assert_eq!(summary.result["sections"][1]["id"], "theme-a");
    let compared = ok(
        &mut s,
        "compare_revisions",
        json!({"project": "village", "from": "r1", "to": "r2"}),
    );
    assert_eq!(compared.result["changed_timeline"], json!(["markers"]));

    ok(
        &mut s,
        "export_project",
        json!({"job_id": "plain", "project": "village", "destination": "out/plain", "render_audio": "none"}),
    );
    ok(
        &mut s,
        "export_project",
        json!({"job_id": "sections", "project": "village", "destination": "out/sections", "render_audio": "none", "sections_as_markers": true}),
    );
    let meta = |dir: &str| -> Vec<(u64, String, String)> {
        let bytes = std::fs::read(space.path.join(format!("out/{dir}/village.mid"))).unwrap();
        let smf = midly::Smf::parse(&bytes).unwrap();
        let mut found = Vec::new();
        let mut tick = 0;
        for event in &smf.tracks[0] {
            tick += u64::from(event.delta.as_int());
            let (kind, text) = match event.kind {
                midly::TrackEventKind::Meta(midly::MetaMessage::Marker(t)) => ("marker", t),
                midly::TrackEventKind::Meta(midly::MetaMessage::CuePoint(t)) => ("cue", t),
                _ => continue,
            };
            found.push((
                tick,
                kind.to_owned(),
                String::from_utf8_lossy(text).into_owned(),
            ));
        }
        found
    };
    let m = |tick: u64, kind: &str, text: &str| (tick, kind.to_owned(), text.to_owned());
    assert_eq!(
        meta("plain"),
        vec![
            m(0, "marker", "start"),
            m(3840, "marker", "loop"),
            m(3840, "marker", "loopStart"),
            m(9600, "cue", "drums in"),
            m(15360, "marker", "loopEnd"),
        ]
    );
    assert_eq!(
        meta("sections"),
        vec![
            m(0, "marker", "start"),
            m(0, "marker", "intro"),
            m(3840, "marker", "loop"),
            m(3840, "marker", "theme-a"),
            m(3840, "marker", "loopStart"),
            m(9600, "cue", "drums in"),
            m(9600, "marker", "theme-b"),
            m(15360, "marker", "loopEnd"),
        ]
    );
    let report: Value = serde_json::from_slice(
        &std::fs::read(space.path.join("out/sections/report.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(report["sections"]["items"].as_array().unwrap().len(), 3);
    let plain: Value =
        serde_json::from_slice(&std::fs::read(space.path.join("out/plain/report.json")).unwrap())
            .unwrap();
    assert_eq!(plain.get("sections"), None);

    // Source-backed projects keep their original markers.
    ok(
        &mut s,
        "import_midi",
        json!({"job_id": "import", "project": "src", "path": "out/plain/village.mid"}),
    );
    let blocked = s.call(
        "edit_project",
        json!({"project": "src", "request": {"job_id": "src-markers", "base_revision": "r1", "scope": {"timeline": true},
            "operations": [{"op": "set_markers", "markers": [{"at": 0, "text": "new"}]}]}}),
    );
    assert_eq!(blocked.status, Status::Blocked);
    assert_eq!(
        refused(
            &mut s,
            "export_project",
            json!({"job_id": "src-sections", "project": "src", "destination": "out/src", "sections_as_markers": true})
        ),
        ("invalid_range".into(), "/sections_as_markers".into())
    );
}

#[test]
fn pre_master_sum_checks_say_whether_the_master_stage_changes_the_mix() {
    let space = Workspace::new("master-identity");
    let mut s = Service::new(&space.path);
    mixed_village(&mut s);
    // r2 has a -1.5 dB master, a reverb and a limiter on the master.
    let changed = ok(
        &mut s,
        "render_batch",
        json!({"job_id": "pre", "project": "village", "stems": {"by": "voice"}, "stem_master": "pre_master"}),
    );
    let check = &changed.result["spans"][0]["sum_check"];
    assert_eq!(check["sums_within_rounding"], true);
    assert_eq!(check["master_is_identity"], false);
    assert_eq!(check["sums_to_mixdown"], false);
    assert!(
        check["sums_to"]
            .as_str()
            .unwrap()
            .contains("master input only")
    );
    assert!(
        check["mixdown_vs_reference_max_abs_difference"]
            .as_f64()
            .unwrap()
            > 0.01
    );

    // A mixer whose master stage is neutral: pre-master stems sum to the file.
    ok(
        &mut s,
        "edit_project",
        json!({"project": "village", "branch": true, "request": {"job_id": "neutral", "base_revision": "r1",
            "operations": [
            {"op": "set_mix", "target": {"kind": "voice", "id": "melody"}, "volume_db": -3.0}]}}),
    );
    let neutral = ok(
        &mut s,
        "render_batch",
        json!({"job_id": "pre-neutral", "project": "village", "stems": {"by": "voice"}, "stem_master": "pre_master"}),
    );
    let check = &neutral.result["spans"][0]["sum_check"];
    assert_eq!(check["master_is_identity"], true);
    assert_eq!(check["sums_to_mixdown"], true);
    assert_eq!(check["mixdown_vs_reference_max_abs_difference"], 0.0);
    assert!(check.get("sums_to").is_none());

    // Through the master the stems sum to the mixdown file.
    let through = ok(
        &mut s,
        "render_batch",
        json!({"job_id": "through", "project": "village", "stems": {"by": "voice"}}),
    );
    let check = &through.result["spans"][0]["sum_check"];
    assert_eq!(check["sums_to_mixdown"], true);
    assert!(check.get("master_is_identity").is_none());
}

#[test]
fn encoded_sum_checks_measure_the_written_codes() {
    let space = Workspace::new("encoded");
    let mut s = Service::new(&space.path);
    mixed_village(&mut s);
    for format in ["pcm16", "pcm24"] {
        let batch = ok(
            &mut s,
            "render_batch",
            json!({"job_id": format!("codes-{format}"), "project": "village", "stems": {"by": "voice"},
                "spans": [{"kind": "section", "id": "theme-a"}], "format": format}),
        );
        let listed = renders(&batch);
        let mixdown = codes(&path(&listed[0]));
        let stems: Vec<Vec<i32>> = listed[1..].iter().map(|r| codes(&path(r))).collect();
        let measured = (0..mixdown.len())
            .map(|n| {
                (stems.iter().map(|s| i64::from(s[n])).sum::<i64>() - i64::from(mixdown[n])).abs()
            })
            .max()
            .unwrap();
        let encoded = &batch.result["spans"][0]["sum_check"]["encoded"];
        assert_eq!(encoded["format"], format);
        assert_eq!(encoded["max_abs_difference_codes"], measured, "{format}");
        assert_eq!(encoded["clamped_samples"], 0);
        assert!(measured <= encoded["rounding_bound_codes"].as_i64().unwrap());
        // Three stems and the mixdown round on their own: at most two codes.
        assert!(encoded["rounding_bound_codes"].as_i64().unwrap() <= 2);
    }
    let float = ok(
        &mut s,
        "render_batch",
        json!({"job_id": "codes-float", "project": "village", "stems": {"by": "voice"}, "format": "float32"}),
    );
    let check = &float.result["spans"][0]["sum_check"];
    assert_eq!(
        check["encoded"]["max_abs_difference"],
        check["max_abs_difference"]
    );
}

#[test]
fn silent_stems_are_flagged() {
    let space = Workspace::new("silent");
    let mut s = Service::new(&space.path);
    village(&mut s);
    ok(
        &mut s,
        "edit_project",
        json!({"project": "village", "request": {"job_id": "pad", "base_revision": "r1",
            "operations": [{"op": "add_voice", "voice": {"id": "pad", "role": "pad"}}]}}),
    );
    let batch = ok(
        &mut s,
        "render_batch",
        json!({"job_id": "stems", "project": "village", "stems": {"by": "voice"}}),
    );
    let silent: Vec<Value> = renders(&batch)
        .iter()
        .filter(|r| r.get("silent") == Some(&json!(true)))
        .map(|r| r["render"]["part"].clone())
        .collect();
    assert_eq!(silent, vec![json!({"kind": "voice", "id": "pad"})]);
    // compare_revisions lists every compared category, empty or not.
    let compared = ok(
        &mut s,
        "compare_revisions",
        json!({"project": "village", "from": "r1", "to": "r2"}),
    );
    assert_eq!(compared.result["added_voices"], json!(["pad"]));
    assert_eq!(compared.result["changed_mix"], json!([]));
    assert_eq!(compared.result["changed_clips"], json!([]));
}

#[test]
fn cut_tails_end_at_the_section_and_regenerate() {
    let space = Workspace::new("tail-cut");
    let mut s = Service::new(&space.path);
    mixed_village(&mut s);
    let kept_args = json!({"job_id": "kept", "project": "village",
        "spans": [{"kind": "section", "id": "intro"}], "sample_rate": 32000});
    let kept = ok(&mut s, "render_batch", kept_args.clone());
    assert!(
        kept.result["spans"][0]["timing"]["tail_seconds"]
            .as_f64()
            .unwrap()
            > 0.0
    );
    assert!(kept.result["spans"][0]["timing"].get("tail").is_none());
    // An explicit default tail is the same request.
    let mut explicit = kept_args;
    explicit["tail"] = json!("keep");
    assert!(ok(&mut s, "render_batch", explicit).replayed);

    let cut = ok(
        &mut s,
        "render_batch",
        json!({"job_id": "cut", "project": "village", "spans": [{"kind": "sections"}],
            "stems": {"by": "voice"}, "tail": "cut", "sample_rate": 32000}),
    );
    let listed = renders(&cut);
    for (i, span) in cut.result["spans"].as_array().unwrap().iter().enumerate() {
        let timing = &span["timing"];
        assert_eq!(timing["tail"], "cut");
        assert_eq!(timing["tail_truncated"], false);
        let content = timing["content_seconds"].as_f64().unwrap();
        assert!((timing["total_seconds"].as_f64().unwrap() - content).abs() < 1.0 / 32000.0);
        assert_eq!(span["sum_check"]["sums_within_rounding"], true);
        for render in &listed[i * 4..i * 4 + 4] {
            assert_eq!(render["render"]["region"], span["region"]);
            assert_eq!(render["render"]["tail_cut"], true);
            assert_eq!(
                render["render"]["length_samples"].as_u64().unwrap() as f64,
                (content * 32000.0).ceil()
            );
        }
    }

    // The export report names what every delivered WAV holds; the bundle
    // regenerates cut renders by hash.
    let id = listed[1]["render"]["id"].as_str().unwrap().to_owned();
    ok(
        &mut s,
        "export_project",
        json!({"job_id": "export", "project": "village", "destination": "out/cut",
            "render_audio": "listed", "render_ids": [id]}),
    );
    let folder = space.path.join("out/cut");
    let report: Value =
        serde_json::from_slice(&std::fs::read(folder.join("report.json")).unwrap()).unwrap();
    let wavs: Vec<&Value> = report["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["kind"] == "wav")
        .collect();
    assert_eq!(wavs.len(), 1);
    assert_eq!(
        std::path::Path::new(wavs[0]["path"].as_str().unwrap()),
        std::path::PathBuf::from(format!("renders/{id}.wav"))
    );
    assert_eq!(wavs[0]["render"], id);
    assert_eq!(wavs[0]["region"], "intro");
    assert_eq!(wavs[0]["part"], listed[1]["render"]["part"]);
    assert_eq!(wavs[0]["format"], "pcm16");
    assert_eq!(wavs[0]["tail_cut"], true);

    let other = Workspace::new("tail-cut-import");
    std::fs::create_dir_all(other.path.join("in")).unwrap();
    std::fs::copy(
        folder.join("village.project.json"),
        other.path.join("in/village.project.json"),
    )
    .unwrap();
    let mut t = Service::new(&other.path);
    let imported = ok(
        &mut t,
        "import_project",
        json!({"job_id": "import", "path": "in/village.project.json", "missing_renders": "regenerate"}),
    );
    let checks = imported.result["regenerated_checks"].as_array().unwrap();
    assert!(!checks.is_empty());
    assert!(
        checks
            .iter()
            .all(|c| c["checked"] == "sha256 equals the recorded render")
    );
}
