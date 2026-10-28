//! Retained CRI ADX streams are interpreted with located fields, decoded
//! readback, loop-aware renders and encoded readback; refusals keep the
//! original. Every stream here is synthetic, written by the test.
use serde_json::{Value, json};
use tunemend::adx::{self, Adx, Arithmetic, EncodeParams};
use tunemend::service::{Service, Status, ToolResponse};

fn workspace(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("tunemend-adx-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn ok(service: &mut Service, tool: &str, args: Value) -> ToolResponse {
    let response = service.call(tool, args);
    assert_eq!(response.status, Status::Ok, "{response:?}");
    response
}

fn blocked(service: &mut Service, tool: &str, args: Value) -> ToolResponse {
    let response = service.call(tool, args);
    assert_eq!(response.status, Status::Blocked, "{response:?}");
    response
}

fn tone(frames: usize, period: f64, amplitude: f64) -> Vec<i16> {
    (0..frames)
        .map(|i| ((i as f64 * std::f64::consts::TAU / period).sin() * amplitude).round() as i16)
        .collect()
}

/// A stereo looping stream with sector-aligned loop start and length.
fn stereo_loop() -> Vec<u8> {
    let left = tone(4000, 50.0, 10000.0);
    let right = tone(4000, 25.0, 6000.0);
    adx::encode(
        &[left, right],
        EncodeParams {
            sample_rate: 22050,
            highpass: 500,
            looping: Some((800, 3800)),
            sector_bytes: Some(2048),
            alignment_samples: 0,
        },
    )
    .unwrap()
}

fn load(service: &mut Service, root: &std::path::Path, name: &str, bytes: &[u8]) -> String {
    std::fs::write(root.join(name), bytes).unwrap();
    let loaded = ok(
        service,
        "load_instrument_source",
        json!({"job_id": format!("load-{name}"), "path": name}),
    );
    loaded.result["source"].as_str().unwrap().to_owned()
}

fn wav_pcm(path: &std::path::Path) -> (u16, u32, Vec<i16>, Value) {
    let bytes = std::fs::read(path).unwrap();
    let wav = tunemend::wav::parse(&bytes).unwrap();
    let pcm = wav
        .data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|w| i16::from_le_bytes(*w))
        .collect();
    (wav.channels, wav.sample_rate, pcm, wav.smpl_report())
}

#[test]
fn retained_stream_is_interpreted_with_offsets_and_decoded_readback() {
    let root = workspace("inspect");
    let mut service = Service::new(&root);
    let bytes = stereo_loop();
    let id = load(&mut service, &root, "song.adx", &bytes);
    let loaded = ok(
        &mut service,
        "load_instrument_source",
        json!({"job_id": "load-song.adx", "path": "song.adx"}),
    );
    assert!(loaded.replayed);
    let hint = &loaded.result["discovery"]["formats"][0];
    assert_eq!(hint["format"], "adx");
    assert_eq!(hint["channels"], 2);
    assert_eq!(hint["loop"], "enabled");

    let summary = ok(&mut service, "inspect_adx", json!({"source": id}));
    let r = &summary.result;
    assert_eq!(r["version"], 3);
    assert_eq!(r["sample_rate"], 22050);
    assert_eq!(r["total_samples"], 4000);
    assert_eq!(r["loop"]["start_sample"], 800);
    assert_eq!(r["loop"]["end_sample_exclusive"], 3800);
    assert_eq!(r["loop"]["start_byte"].as_u64().unwrap() % 2048, 0);
    assert_eq!(r["loop"]["start_byte_consistent"], true);
    assert_eq!(r["end"]["marker"], "0x8001");
    assert_eq!(r["trailing_bytes"], 0);
    assert_eq!(r["coefficients"]["fixed_12"], json!([6569, -2634]));
    // Zero padding between the loop fields and the marker is exposed.
    let unknown = r["unknown_header_bytes"].as_array().unwrap();
    assert_eq!(unknown[0]["offset"], 0x2C);
    assert_eq!(unknown[0]["nonzero_bytes"], 0);

    let header = ok(
        &mut service,
        "inspect_adx",
        json!({"source": id, "view": "header"}),
    );
    let fields = header.result["fields"].as_array().unwrap();
    let field = |name: &str| fields.iter().find(|f| f["name"] == name).unwrap().clone();
    assert_eq!(field("flags")["offset"], 19);
    assert_eq!(field("loop_begin_sample")["offset"], 0x1C);
    assert_eq!(field("loop_end_byte")["offset"], 0x28);
    assert_eq!(field("highpass_frequency")["raw_hex"], "01f4");

    let blocks = ok(
        &mut service,
        "inspect_adx",
        json!({"source": id, "view": "blocks", "offset": 124, "limit": 4}),
    );
    let rows = blocks.result["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "125 rows hold 4000 frames");
    assert_eq!(rows[0]["frames"], 32);
    assert_eq!(blocks.result["next_cursor"], Value::Null);
    let offset = rows[0]["blocks"][1]["offset"].as_u64().unwrap() as usize;
    let parsed = Adx::parse(&bytes).unwrap();
    assert_eq!(offset, parsed.block_offset(124, 1));

    // Decoded readback: inspection values equal the codec's own decode.
    let (pcm, _) = adx::decode_stream(&parsed, Arithmetic::Fixed12).unwrap();
    let page = ok(
        &mut service,
        "inspect_adx",
        json!({"source": id, "view": "samples", "offset": 3790, "limit": 20}),
    );
    let frames = page.result["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 20);
    assert_eq!(page.result["next_cursor"], 3810);
    for f in frames {
        let i = f["frame"].as_u64().unwrap() as usize;
        for c in 0..2 {
            let v = &f["channels"][c];
            assert_eq!(v["value"], pcm[i * 2 + c]);
            let (byte, high, code) = parsed.nibble(i / 32, c, i % 32);
            assert_eq!(v["byte"], byte);
            assert_eq!(v["nibble"], if high { "high" } else { "low" });
            assert_eq!(v["code"], code);
        }
    }

    let exported = ok(
        &mut service,
        "export_instrument_source",
        json!({"job_id": "export", "source": id, "destination": "out/original"}),
    );
    assert!(!exported.artifacts.is_empty());
    assert_eq!(
        std::fs::read(root.join("out/original/original.bin")).unwrap(),
        bytes
    );
    // A new service over the same workspace reads the retained stream.
    std::fs::remove_file(root.join("song.adx")).unwrap();
    let mut restarted = Service::new(&root);
    ok(&mut restarted, "inspect_adx", json!({"source": id}));
}

#[test]
fn renders_keep_the_loop_and_check_reentry() {
    let root = workspace("render");
    let mut service = Service::new(&root);
    let bytes = stereo_loop();
    let id = load(&mut service, &root, "song.adx", &bytes);
    let parsed = Adx::parse(&bytes).unwrap();
    let (pcm, _) = adx::decode_stream(&parsed, Arithmetic::Fixed12).unwrap();

    let stream = ok(
        &mut service,
        "render_adx",
        json!({"job_id": "stream", "source": id, "destination": "out/stream"}),
    );
    let audio = &stream.result["measure_with"]["audio"];
    assert_eq!(
        std::path::Path::new(audio["path"].as_str().unwrap()),
        std::path::Path::new("out/stream").join("decoded.wav")
    );
    assert_eq!(audio["expected_sha256"], stream.result["wav"]["sha256"]);
    ok(
        &mut service,
        "measure_audio",
        json!({"audio": audio.clone()}),
    );
    // decoded.wav is retained: encode_adx takes it without another load.
    let decoded_source = stream.result["decoded_source"].as_str().unwrap().to_owned();
    let mut encode = stream.result["encode_adx"]["arguments"].clone();
    assert_eq!(encode["loop_frames"], json!({"start": 800, "end": 3800}));
    encode["job_id"] = json!("reencode");
    encode["destination"] = json!("out/reencode");
    encode["accept_changes"] = json!(["adpcm_quantization"]);
    let reencoded = ok(&mut service, "encode_adx", encode);
    assert_eq!(reencoded.result["from"], decoded_source);
    assert_eq!(
        reencoded.result["readback"]["loop"],
        json!({"start": 800, "end_exclusive": 3800})
    );
    let (channels, rate, decoded, smpl) = wav_pcm(&root.join("out/stream/decoded.wav"));
    assert_eq!((channels, rate), (2, 22050));
    assert_eq!(decoded, pcm);
    assert_eq!(smpl["loops"][0]["start_frame"], 800);
    assert_eq!(smpl["loops"][0]["end_frame_inclusive"], 3799);
    assert_eq!(
        std::fs::read(root.join("out/stream/original.adx")).unwrap(),
        bytes
    );
    let replay = ok(
        &mut service,
        "render_adx",
        json!({"job_id": "stream", "source": id, "destination": "out/stream"}),
    );
    assert!(replay.replayed);

    let excerpt = ok(
        &mut service,
        "render_adx",
        json!({"job_id": "frames", "source": id, "destination": "out/frames",
               "span": {"kind": "frames", "start": 700, "end": 3900}}),
    );
    assert_eq!(excerpt.result["frames"], 3200);
    let (_, _, cut, smpl) = wav_pcm(&root.join("out/frames/decoded.wav"));
    assert_eq!(cut, pcm[700 * 2..3900 * 2]);
    assert_eq!(smpl["loops"][0]["start_frame"], 100);

    let looped = ok(
        &mut service,
        "render_adx",
        json!({"job_id": "loop", "source": id, "destination": "out/loop",
               "span": {"kind": "loop", "passes": 3}}),
    );
    let (_, _, unrolled, smpl) = wav_pcm(&root.join("out/loop/decoded.wav"));
    assert_eq!(smpl, Value::Null);
    assert_eq!(unrolled.len(), (3800 + 2 * 3000) * 2);
    assert_eq!(unrolled[..3800 * 2], pcm[..3800 * 2]);
    let check = &looped.result["reentry_check"];
    assert_eq!(check["history_equal"], false);
    for c in check["second_traversal"]["channels"].as_array().unwrap() {
        // A whole number of periods: the traversal settles back onto the first pass.
        assert!(c["max_abs_difference"].as_i64().unwrap() < 1500, "{c}");
        assert!(!c["identical_from_frame"].is_null(), "{c}");
    }
    // The third pass continues from the second with carried history.
    let mut decoder = adx::Decoder::new(&parsed, Arithmetic::Fixed12);
    let mut expect = vec![];
    decoder.decode(0, 3800, &mut expect);
    decoder.decode(800, 3800, &mut expect);
    decoder.decode(800, 3800, &mut expect);
    assert_eq!(unrolled, expect);

    let float = ok(
        &mut service,
        "render_adx",
        json!({"job_id": "float", "source": id, "destination": "out/float",
               "arithmetic": "float_reference"}),
    );
    assert_eq!(float.result["arithmetic"]["name"], "float_reference");
    let fixed = ok(
        &mut service,
        "inspect_adx",
        json!({"source": id, "arithmetic": "fixed_12"}),
    );
    assert_eq!(fixed.result["arithmetic"]["name"], "fixed_12");
    let other = service.call(
        "inspect_adx",
        json!({"source": id, "arithmetic": "fixed12"}),
    );
    assert_eq!(
        other.diagnostics[0].code,
        tunemend::DiagnosticCode::SchemaMismatch
    );

    blocked(
        &mut service,
        "render_adx",
        json!({"job_id": "bad-frames", "source": id, "destination": "out/x",
               "span": {"kind": "frames", "start": 10, "end": 4001}}),
    );
    blocked(
        &mut service,
        "render_adx",
        json!({"job_id": "bad-passes", "source": id, "destination": "out/y",
               "span": {"kind": "loop", "passes": 0}}),
    );
    let one_shot = adx::encode(
        &[tone(100, 10.0, 1000.0)],
        EncodeParams {
            sample_rate: 22050,
            highpass: 500,
            looping: None,
            sector_bytes: None,
            alignment_samples: 0,
        },
    )
    .unwrap();
    let one = load(&mut service, &root, "one.adx", &one_shot);
    let refused = blocked(
        &mut service,
        "render_adx",
        json!({"job_id": "no-loop", "source": one, "destination": "out/z",
               "span": {"kind": "loop", "passes": 2}}),
    );
    assert_eq!(refused.diagnostics[0].location, "/span");
}

#[test]
fn unadmitted_streams_refuse_at_their_byte_and_stay_retained() {
    let root = workspace("refuse");
    let mut service = Service::new(&root);
    let good = stereo_loop();
    for (name, at, value, location) in [
        ("encrypted.adx", 19usize, 0x08u8, "/source/bytes/19"),
        ("type4.adx", 4, 4, "/source/bytes/4"),
        ("ahx.adx", 4, 0x10, "/source/bytes/4"),
        ("v6.adx", 18, 6, "/source/bytes/18"),
    ] {
        let mut bytes = good.clone();
        bytes[at] = value;
        let id = load(&mut service, &root, name, &bytes);
        let loaded = ok(
            &mut service,
            "load_instrument_source",
            json!({"job_id": format!("load-{name}"), "path": name}),
        );
        assert!(loaded.result["discovery"]["formats"][0]["unusable"].is_string());
        let refused = blocked(&mut service, "inspect_adx", json!({"source": id}));
        assert_eq!(refused.diagnostics[0].location, location, "{name}");
        assert_eq!(refused.result["retained"], true);
        blocked(
            &mut service,
            "render_adx",
            json!({"job_id": format!("render-{name}"), "source": id, "destination": format!("out/{name}")}),
        );
        ok(
            &mut service,
            "export_instrument_source",
            json!({"job_id": format!("export-{name}"), "source": id, "destination": format!("out/orig-{name}")}),
        );
        assert_eq!(
            std::fs::read(root.join(format!("out/orig-{name}/original.bin"))).unwrap(),
            bytes
        );
    }
    let encrypted = service.call(
        "inspect_adx",
        json!({"source": tunemend::store::sha256_hex(&{
            let mut b = good.clone();
            b[19] = 8;
            b
        })}),
    );
    assert!(
        encrypted.summary.contains("encrypted"),
        "{}",
        encrypted.summary
    );
    let truncated = load(&mut service, &root, "short.adx", &good[..3000]);
    let refused = blocked(&mut service, "inspect_adx", json!({"source": truncated}));
    assert_eq!(refused.diagnostics[0].location, "/source/bytes/3000");
}

fn pcm16_wav(channels: &[Vec<i16>], rate: u32, smpl: Option<(u32, u32)>) -> Vec<u8> {
    let frames = channels[0].len();
    let interleaved: Vec<i16> = (0..frames)
        .flat_map(|i| channels.iter().map(move |c| c[i]))
        .collect();
    adx::pcm16_wav(&interleaved, channels.len(), rate, smpl)
}

#[test]
fn encoding_reports_losses_reads_back_and_keeps_the_loop() {
    let root = workspace("encode");
    let mut service = Service::new(&root);
    let left = tone(6400, 64.0, 12000.0);
    let right = tone(6400, 32.0, 5000.0);
    let wav = pcm16_wav(&[left.clone(), right.clone()], 32000, Some((1024, 6399)));
    let id = load(&mut service, &root, "source.wav", &wav);
    let request = |job: &str, accept: Value, looping: Value| {
        json!({"job_id": job, "source": id, "highpass_frequency": 500,
               "loop_frames": looping, "sector_bytes": 2048,
               "accept_changes": accept, "destination": format!("out/{job}")})
    };
    let refused = blocked(
        &mut service,
        "encode_adx",
        request("plain", json!([]), json!({"start": 1024, "end": 6400})),
    );
    assert_eq!(refused.result["written"], false);
    assert_eq!(refused.result["changes"][0]["kind"], "adpcm_quantization");
    assert_eq!(refused.diagnostics.len(), 1, "the smpl loop is carried");
    assert!(!root.join("out/plain").exists());

    let misaligned = blocked(
        &mut service,
        "encode_adx",
        request(
            "misaligned",
            json!(["adpcm_quantization"]),
            json!({"start": 1000, "end": 6400}),
        ),
    );
    assert_eq!(misaligned.diagnostics[0].location, "/loop_frames/start");
    assert!(
        misaligned.summary.contains("992 or 1024"),
        "{}",
        misaligned.summary
    );

    // Loop start padding writes the lead-in count to the alignment field.
    let padded = request(
        "padded",
        json!(["adpcm_quantization"]),
        json!({"start": 1000, "end": 6400}),
    );
    let mut padded_args = padded.clone();
    padded_args["loop_start_padding"] = json!(true);
    padded_args["accept_changes"] = json!(["adpcm_quantization", "wav_metadata_not_carried"]);
    let refused = blocked(&mut service, "encode_adx", padded_args.clone());
    assert!(
        refused.summary.contains("lead_in_padding"),
        "{}",
        refused.summary
    );
    padded_args["job_id"] = json!("padded-2");
    padded_args["destination"] = json!("out/padded-2");
    padded_args["accept_changes"] = json!([
        "adpcm_quantization",
        "wav_metadata_not_carried",
        "lead_in_padding"
    ]);
    let padded = ok(&mut service, "encode_adx", padded_args);
    assert_eq!(padded.result["readback"]["frames"], 6424);
    assert_eq!(
        padded.result["readback"]["loop"],
        json!({"start": 1024, "end_exclusive": 6424})
    );
    let padded_id = padded.result["source"].as_str().unwrap().to_owned();
    let summary = ok(&mut service, "inspect_adx", json!({"source": padded_id}));
    assert_eq!(summary.result["loop"]["alignment_samples"], 24);
    assert_eq!(summary.result["loop"]["alignment_lead_in_silent"], true);

    let encoded = ok(
        &mut service,
        "encode_adx",
        request(
            "looped",
            json!(["adpcm_quantization"]),
            json!({"start": 1024, "end": 6400}),
        ),
    );
    let new_id = encoded.result["source"].as_str().unwrap().to_owned();
    assert_eq!(encoded.result["from"], id);
    assert_eq!(
        encoded.result["readback"]["loop"],
        json!({"start": 1024, "end_exclusive": 6400})
    );
    for c in encoded.result["changes"][0]["channels"].as_array().unwrap() {
        assert!(c["snr_db"].as_f64().unwrap() > 25.0, "{c}");
    }
    assert!(encoded.result["loop_reentry"]["second_traversal"].is_object());
    let bytes = std::fs::read(root.join("out/looped/encoded.adx")).unwrap();
    assert_eq!(bytes.len() % 2048, 0);
    assert_eq!(tunemend::store::sha256_hex(&bytes), new_id);
    // The new stream is a retained source: inspection and render read it back.
    let summary = ok(&mut service, "inspect_adx", json!({"source": new_id}));
    assert_eq!(summary.result["sample_rate"], 32000);
    assert_eq!(
        summary.result["loop"]["start_byte"].as_u64().unwrap() % 2048,
        0
    );
    ok(
        &mut service,
        "render_adx",
        json!({"job_id": "render-new", "source": new_id, "destination": "out/render-new"}),
    );
    let (_, _, rendered, _) = wav_pcm(&root.join("out/render-new/decoded.wav"));
    let (_, _, readback, smpl) = wav_pcm(&root.join("out/looped/decoded.wav"));
    assert_eq!(rendered, readback);
    assert_eq!(smpl["loops"][0]["end_frame_inclusive"], 6399);
    let sources = ok(
        &mut service,
        "inspect_instrument_source",
        json!({"view": "sources"}),
    );
    let derived = sources.result["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["source"] == new_id)
        .unwrap()
        .clone();
    assert_eq!(derived["derived_from"]["tool"], "encode_adx");

    // Dropping the smpl loop is a reported change of its own.
    let one_shot = blocked(
        &mut service,
        "encode_adx",
        request("one-shot", json!(["adpcm_quantization"]), Value::Null),
    );
    assert_eq!(
        one_shot.diagnostics[0].message,
        "wav_metadata_not_carried is reported and not accepted"
    );

    // Silence encodes exactly: no change to accept.
    let silent = pcm16_wav(&[vec![0; 100]], 22050, None);
    let silent_id = load(&mut service, &root, "silent.wav", &silent);
    let exact = ok(
        &mut service,
        "encode_adx",
        json!({"job_id": "silent", "source": silent_id, "highpass_frequency": 500, "destination": "out/silent"}),
    );
    assert_eq!(exact.result["changes"], json!([]));

    let float = tunemend::wav::build(3, 1, 22050, 32, None, &[0u8; 64], &[]);
    let float_id = load(&mut service, &root, "float.wav", &float);
    let refused = blocked(
        &mut service,
        "encode_adx",
        json!({"job_id": "float", "source": float_id, "highpass_frequency": 500, "destination": "out/float"}),
    );
    assert!(refused.summary.contains("16-bit integer PCM"));
}

#[test]
fn loop_ranking_predicts_measured_reentry_and_applies_nothing() {
    let root = workspace("rank");
    let mut service = Service::new(&root);
    // A cosine of period 64 after silence: whole-period loops re-enter cleanly.
    let mut pcm = vec![0i16; 300];
    pcm.extend(
        (0..64 * 60)
            .map(|i| ((i as f64 * std::f64::consts::TAU / 64.0).cos() * 11000.0).round() as i16),
    );
    let frames = pcm.len() as u32;
    let wav = pcm16_wav(&[pcm], 22050, None);
    let id = load(&mut service, &root, "theme.wav", &wav);
    let ranked = ok(
        &mut service,
        "rank_adx_loops",
        json!({"source": id, "highpass_frequency": 500, "start": {"from": 0, "to": 1000},
               "end": {"from": frames - 200, "to": frames}, "min_length": 1000, "limit": 3}),
    );
    let r = &ranked.result;
    assert_eq!(r["starts"], 32);
    assert_eq!(r["pairs_evaluated"], 32 * 201);
    let best = &r["candidates"][0];
    let lf = best["loop_frames"].clone();
    let (start, end) = (lf["start"].as_u64().unwrap(), lf["end"].as_u64().unwrap());
    assert_eq!(start % 32, 0);
    assert!(start >= 320, "after the silence: {best}");
    assert_eq!((end - start) % 64, 0, "{best}");
    assert!(best["predicted_reentry_difference"].as_f64().unwrap() < 1.0);
    // Nothing was written: the ranking is read-only.
    assert!(!root.join("out").exists());

    // The prediction of a poor pair agrees with the encoded loop's measurement.
    let poor = ok(
        &mut service,
        "rank_adx_loops",
        json!({"source": id, "highpass_frequency": 500, "start": {"from": 640, "to": 640},
               "end": {"from": frames - 16, "to": frames - 16}}),
    );
    let predicted = poor.result["candidates"][0]["predicted_reentry_difference"]
        .as_f64()
        .unwrap();
    let encoded = ok(
        &mut service,
        "encode_adx",
        json!({"job_id": "poor", "source": id, "highpass_frequency": 500,
               "loop_frames": {"start": 640, "end": frames - 16},
               "accept_changes": ["adpcm_quantization"], "destination": "out/poor"}),
    );
    let measured =
        encoded.result["loop_reentry"]["second_traversal"]["channels"][0]["max_abs_difference"]
            .as_f64()
            .unwrap();
    assert!(predicted > 1000.0);
    assert!(
        (predicted - measured).abs() < 0.1 * measured + 50.0,
        "{predicted} vs {measured}"
    );

    let refused = blocked(
        &mut service,
        "rank_adx_loops",
        json!({"source": id, "highpass_frequency": 500, "start": {"from": 1, "to": 31},
               "end": {"from": 2000, "to": 2100}}),
    );
    assert_eq!(refused.diagnostics[0].location, "/start");
    let long = pcm16_wav(&[vec![0; 8000]], 22050, None);
    let long = load(&mut service, &root, "long.wav", &long);
    let refused = blocked(
        &mut service,
        "rank_adx_loops",
        json!({"source": long, "highpass_frequency": 500, "start": {"from": 0, "to": 8000},
               "end": {"from": 0, "to": 8000}}),
    );
    assert!(refused.summary.contains("narrow"), "{}", refused.summary);
}

#[test]
fn capabilities_describe_adx_streams() {
    let mut service = Service::new(workspace("capabilities"));
    let caps = ok(
        &mut service,
        "get_capabilities",
        json!({"sections": ["adx_streams"]}),
    );
    let section = &caps.result["adx_streams"];
    assert_eq!(section["arithmetic"]["default"], "fixed_12");
    assert!(section["refused"].as_str().unwrap().contains("0x08"));
}
