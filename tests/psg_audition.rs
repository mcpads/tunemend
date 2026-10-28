//! A retained VGM is interpreted without loss and its PSG stream renders the
//! same as the equivalent explicit writes; malformed files refuse with offsets.
use serde_json::{Value, json};
use tunemend::service::{Service, Status, ToolResponse};

fn workspace(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("tunemend-psg-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn ok(service: &mut Service, tool: &str, args: Value) -> ToolResponse {
    let response = service.call(tool, args);
    assert_eq!(response.status, Status::Ok, "{response:?}");
    response
}

const CLOCK: u32 = 3_579_545;

/// An original synthetic VGM 1.71 file: a 0x100-byte header, `body`, a GD3 tag.
fn vgm(body: &[u8], loop_command_offset: Option<usize>) -> Vec<u8> {
    let mut bytes = vec![0u8; 0x100];
    bytes[..4].copy_from_slice(b"Vgm ");
    bytes[8..12].copy_from_slice(&0x171u32.to_le_bytes());
    bytes[0x0c..0x10].copy_from_slice(&CLOCK.to_le_bytes());
    bytes[0x28..0x2a].copy_from_slice(&9u16.to_le_bytes());
    bytes[0x2a] = 16;
    bytes[0x2c..0x30].copy_from_slice(&7_670_453u32.to_le_bytes());
    bytes[0x34..0x38].copy_from_slice(&(0x100u32 - 0x34).to_le_bytes());
    bytes.extend(body);
    let gd3 = bytes.len();
    bytes[0x14..0x18].copy_from_slice(&(gd3 as u32 - 0x14).to_le_bytes());
    let text: Vec<u8> = "Synthetic\0\0\0\0\0\0\0\0\0\0\0"
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    bytes.extend(b"Gd3 ");
    bytes.extend(0x100u32.to_le_bytes());
    bytes.extend((text.len() as u32).to_le_bytes());
    bytes.extend(text);
    let len = bytes.len() as u32 - 4;
    bytes[4..8].copy_from_slice(&len.to_le_bytes());
    let mut waits = 0u32;
    let mut loop_waits = 0u32;
    let mut i = 0;
    while body[i] != 0x66 {
        let (length, wait) = match body[i] {
            0x61 => (3, u32::from(u16::from_le_bytes([body[i + 1], body[i + 2]]))),
            0x62 => (1, 735),
            0x63 => (1, 882),
            0x70..=0x7f => (1, u32::from(body[i] & 15) + 1),
            0x52 | 0x53 => (3, 0),
            _ => (2, 0),
        };
        waits += wait;
        if loop_command_offset.is_some_and(|l| i >= l) {
            loop_waits += wait;
        }
        i += length;
    }
    bytes[0x18..0x1c].copy_from_slice(&waits.to_le_bytes());
    if let Some(l) = loop_command_offset {
        bytes[0x1c..0x20].copy_from_slice(&((0x100 + l - 0x1c) as u32).to_le_bytes());
        bytes[0x20..0x24].copy_from_slice(&loop_waits.to_le_bytes());
    }
    bytes
}

/// A Mega Drive-like stream: PSG tone and noise with GG stereo, YM2612 writes, waits.
fn body() -> Vec<u8> {
    vec![
        0x50, 0x8e, 0x50, 0x0f, 0x50, 0x92, // tone 0 period 0xfe, attenuation 2
        0x52, 0x28, 0xf0, // YM2612 key-on (not rendered)
        0x61, 0x44, 0x01, // wait 324
        0x50, 0xe4, 0x50, 0xf3, // white noise, rate 0; attenuation 3
        0x4f, 0xf1, // channel 0 both sides, others left only
        0x62, // wait 735 (loop starts at offset 18 = this command)
        0x50, 0xa0, 0x50, 0x08, 0x50, 0xb5, // tone 1 period 0x080
        0x75, // wait 6
        0x53, 0x30, 0x01, 0x50, 0x9f, 0x63, // YM2612 port 1, attenuation 0 off, wait 882
        0x66,
    ]
}

fn wav_frames(path: &std::path::Path) -> Vec<[i16; 2]> {
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(&bytes[36..40], b"data");
    bytes[44..]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| {
            [
                i16::from_le_bytes([c[0], c[1]]),
                i16::from_le_bytes([c[2], c[3]]),
            ]
        })
        .collect()
}

fn load(service: &mut Service, root: &std::path::Path, bytes: &[u8]) -> String {
    std::fs::write(root.join("song.vgm"), bytes).unwrap();
    let loaded = ok(
        service,
        "load_instrument_source",
        json!({"job_id":"load","path":"song.vgm"}),
    );
    assert_eq!(loaded.result["discovery"]["formats"][0]["format"], "vgm");
    loaded.result["source"].as_str().unwrap().to_owned()
}

#[test]
fn vgm_inspection_lists_every_command_with_exact_bytes() {
    let root = workspace("inspect");
    let mut service = Service::new(&root);
    let bytes = vgm(&body(), Some(18));
    let source = load(&mut service, &root, &bytes);
    let summary = ok(&mut service, "inspect_vgm", json!({"source":source})).result;
    assert_eq!(summary["version"], "1.71");
    assert_eq!(summary["total_consistent"], true);
    assert_eq!(summary["loop"]["consistent"], true);
    assert_eq!(summary["loop"]["command_index"], 8);
    assert_eq!(summary["psg"]["config"]["taps"], "0x0009");
    assert_eq!(summary["gd3"]["strings"]["track_name"], "Synthetic");
    let chips: Vec<&str> = summary["chips"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["chip"].as_str().unwrap())
        .collect();
    assert_eq!(chips, ["sn76489", "ym2612"]);
    let layout = summary["layout"].as_array().unwrap();
    assert_eq!(layout.last().unwrap()["end"], bytes.len());

    let commands = ok(
        &mut service,
        "inspect_vgm",
        json!({"source":source,"view":"commands","limit":500}),
    )
    .result;
    let items = commands["items"].as_array().unwrap();
    assert_eq!(commands["total"], 17);
    // Reassembling every command's offset and length covers the stream exactly.
    let mut cursor = 0x100;
    for item in items {
        assert_eq!(item["offset"], cursor);
        cursor += item["length"].as_u64().unwrap() as usize;
    }
    assert_eq!(summary["stream_end"], cursor);
    // The data byte after the tone-0 latch is decoded against that latch.
    assert_eq!(items[1]["bytes"], "50 0f");
    assert_eq!(items[1]["meaning"]["byte"], "data");
    assert_eq!(items[1]["meaning"]["register"], "tone0");
    assert_eq!(items[1]["meaning"]["after"], 0xfe);
    assert_eq!(items[3]["meaning"]["chip"], "YM2612 port 0");
    assert_eq!(items[4]["at_sample"], 0);
    assert_eq!(items[5]["at_sample"], 324);
    let filtered = ok(
        &mut service,
        "inspect_vgm",
        json!({"source":source,"view":"commands","kind":"other_chip_write"}),
    )
    .result;
    assert_eq!(filtered["total"], 2);
    // `offset` counts commands; a byte offset past the end refuses with a hint
    // instead of returning an empty page.
    let past = service.call(
        "inspect_vgm",
        json!({"source":source,"view":"commands","offset":0x106}),
    );
    assert_eq!(past.status, Status::Blocked, "{past:?}");
    assert_eq!(past.diagnostics[0].location, "/offset");
    assert!(
        past.diagnostics[0].message.contains("pass offset 3"),
        "{}",
        past.diagnostics[0].message
    );
    assert_eq!(past.result["byte_offset_command_position"], 3);
    let last_page = ok(
        &mut service,
        "inspect_vgm",
        json!({"source":source,"view":"commands","offset":16}),
    )
    .result;
    assert_eq!(last_page["items"].as_array().unwrap().len(), 1);
    let kinds = ok(
        &mut service,
        "inspect_vgm",
        json!({"source":source,"view":"kinds"}),
    )
    .result;
    let names: Vec<&str> = kinds["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["psg_write", "psg_stereo", "wait", "end", "other_chip_write"]
    );
    let header = ok(
        &mut service,
        "inspect_vgm",
        json!({"source":source,"view":"header"}),
    )
    .result;
    let feedback = header["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "sn76489_feedback")
        .unwrap()
        .clone();
    assert_eq!(feedback["raw"], "09 00");
    assert_eq!(feedback["offset"], 0x28);
}

#[test]
fn vgm_render_equals_the_same_writes_rendered_directly() {
    let root = workspace("render");
    let mut service = Service::new(&root);
    let source = load(&mut service, &root, &vgm(&body(), Some(18)));
    let request = json!({"job_id":"vgm","vgm":source,"destination":"out/vgm"});
    let first = ok(&mut service, "render_psg", request.clone());
    let report = &first.result;
    assert_eq!(report["timing"]["native_rate_exact"], "3579545/16");
    assert_eq!(report["timing"]["wav_header_rate_hz"], 223722);
    assert_eq!(report["writes_applied"], 10);
    let skipped = report["skipped"].as_array().unwrap();
    assert_eq!(skipped[0]["kind"], "other_chip_write");
    assert_eq!(skipped[0]["count"], 2);
    // Wait timing: the noise latch after 324 VGM samples lands at
    // floor(324 * 3579545 / 705600) native samples.
    let writes = report["register_writes"].as_array().unwrap();
    assert_eq!(writes[3]["at_sample"], 324);
    assert_eq!(writes[3]["native_sample"], 324u64 * 3_579_545 / 705_600);
    assert_eq!(writes[3]["effect"]["lfsr_reset"], true);
    assert_eq!(writes[5]["port"], "stereo");
    let total = 324 + 735 + 6 + 882;
    assert_eq!(
        report["output"]["frames"],
        total as u64 * 3_579_545 / 705_600
    );
    assert!(service.call("render_psg", request).replayed);
    let full: Value =
        serde_json::from_slice(&std::fs::read(root.join("out/vgm/render.json")).unwrap()).unwrap();
    assert_eq!(full["register_writes"].as_array().unwrap().len(), 10);
    assert_eq!(full["skipped_commands"].as_array().unwrap().len(), 2);

    let explicit: Vec<Value> = full["register_writes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| {
            json!({"at_native_sample":w["native_sample"],"port":w["port"],
                "data":u8::from_str_radix(&w["data"].as_str().unwrap()[2..], 16).unwrap()})
        })
        .collect();
    ok(
        &mut service,
        "render_psg",
        json!({"job_id":"direct","chip":{"variant":"sega","clock_hz":CLOCK},"writes":explicit,
            "tail_seconds":0.05,"destination":"out/direct"}),
    );
    let from_vgm = wav_frames(&root.join("out/vgm/render.wav"));
    let direct = wav_frames(&root.join("out/direct/render.wav"));
    assert!(direct.len() >= from_vgm.len());
    assert_eq!(&direct[..from_vgm.len()], &from_vgm[..]);
    assert!(
        from_vgm.iter().any(|f| f[0] != f[1]),
        "stereo masking audible"
    );
}

#[test]
fn ranges_and_loop_passes_follow_the_unrolled_timeline() {
    let root = workspace("loop");
    let mut service = Service::new(&root);
    let source = load(&mut service, &root, &vgm(&body(), Some(18)));
    let full = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"two","vgm":source,"loop_passes":2,"destination":"out/two"}),
    )
    .result;
    let (total, loop_len) = (324 + 735 + 6 + 882u64, 735 + 6 + 882u64);
    assert_eq!(full["timing"]["loop"]["unrolled_samples"], total + loop_len);
    let writes = full["writes_applied"].as_u64().unwrap();
    // The loop body holds 4 PSG writes, replayed once more.
    assert_eq!(writes, 10 + 4);
    let range = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"range","vgm":source,"loop_passes":2,
            "range":{"start_sample":1000,"end_sample":2500},"destination":"out/range"}),
    )
    .result;
    let all = wav_frames(&root.join("out/two/render.wav"));
    let part = wav_frames(&root.join("out/range/render.wav"));
    let start = (1000u64 * 3_579_545 / 705_600) as usize;
    assert_eq!(part.len(), (2500u64 * 3_579_545 / 705_600) as usize - start);
    assert_eq!(&all[start..start + part.len()], &part[..]);
    assert_eq!(range["timing"]["range"]["start_native"], start);
    // The four loop-body writes of pass 2 fall after the range: reported, not applied.
    assert_eq!(range["writes_after_range"], 4);
    assert_eq!(range["writes_applied"], 10);
    // A resampled-only render omits the native WAV.
    let resampled = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"44k","vgm":source,"output_sample_rate":44100,"destination":"out/44k"}),
    )
    .result;
    assert_eq!(resampled["resampled"]["target_rate"], 44100);
    assert!(!root.join("out/44k/render.wav").exists());
    assert_eq!(
        wav_frames(&root.join("out/44k/render-44100.wav")).len() as u64,
        resampled["resampled"]["target_samples"].as_u64().unwrap()
    );
    // Resampled ranges sit on the timeline's 44100 Hz grid: adjacent ranges
    // tile the whole render whatever native phase their start falls on.
    let whole = wav_frames(&root.join("out/44k/render-44100.wav"));
    assert_eq!(whole.len() as u64, total);
    let mut tiled = Vec::new();
    for (i, (start, end)) in [(0u64, 1000u64), (1000, 1777), (1777, total)]
        .into_iter()
        .enumerate()
    {
        // Both inner starts fall between native samples.
        assert!(start == 0 || start * 3_579_545 % 705_600 != 0);
        let dest = format!("out/tile-{i}");
        let part = ok(
            &mut service,
            "render_psg",
            json!({"job_id":format!("tile-{i}"),"vgm":source,"output_sample_rate":44100,
                "range":{"start_sample":start,"end_sample":end},"destination":dest}),
        )
        .result;
        let frames = wav_frames(&root.join(&dest).join("render-44100.wav"));
        assert_eq!(frames.len() as u64, end - start);
        assert_eq!(part["resampled"]["target_samples"], end - start);
        // Away from the range edges (the kernel reaches about 36 outputs) the
        // samples are the whole render's, bit for bit.
        let edge = 64usize;
        if frames.len() > 2 * edge {
            let at = start as usize;
            assert_eq!(
                &frames[edge..frames.len() - edge],
                &whole[at + edge..at + frames.len() - edge]
            );
        }
        tiled.extend(frames);
    }
    assert_eq!(tiled.len(), whole.len());
    let refused = service.call(
        "render_psg",
        json!({"job_id":"bad-range","vgm":source,"range":{"start_sample":0,"end_sample":999_999},"destination":"out/bad"}),
    );
    assert_eq!(refused.status, Status::Blocked);
    assert_eq!(refused.diagnostics[0].location, "/range");
}

#[test]
fn malformed_vgm_refuses_with_offsets_and_stays_retained() {
    let root = workspace("malformed");
    let mut service = Service::new(&root);
    let mut bytes = vgm(&body(), None);
    // Cut the stream inside the 0x61 wait at offset 0x100 + 9.
    bytes.truncate(0x100 + 10);
    let len = bytes.len() as u32 - 4;
    bytes[4..8].copy_from_slice(&len.to_le_bytes());
    bytes[0x14..0x18].fill(0);
    let source = load(&mut service, &root, &bytes);
    let refused = service.call("inspect_vgm", json!({"source":source}));
    assert_eq!(refused.status, Status::Blocked);
    assert_eq!(refused.diagnostics[0].location, "/source/bytes/265");
    assert!(refused.diagnostics[0].message.contains("needs 3 bytes"));
    let render = service.call(
        "render_psg",
        json!({"job_id":"r","vgm":source,"destination":"out/r"}),
    );
    assert_eq!(render.diagnostics[0].location, "/vgm/bytes/265");
    ok(
        &mut service,
        "export_instrument_source",
        json!({"job_id":"export","source":source,"destination":"out/original"}),
    );
    assert_eq!(
        std::fs::read(root.join("out/original/original.bin")).unwrap(),
        bytes
    );
}

#[test]
fn explicit_notes_render_and_undeclared_context_refuses() {
    let root = workspace("notes");
    let mut service = Service::new(&root);
    let notes = json!([
        {"channel":0,"start_seconds":0.0,"duration_seconds":0.05,"pitch":{"kind":"midi_note","note":69,"a4_hz":440.0},"attenuation":0},
        {"channel":3,"start_seconds":0.02,"duration_seconds":0.03,"noise":{"mode":"periodic","rate":2},"attenuation":4}
    ]);
    let rendered = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"notes","chip":{"variant":"sn76489","clock_hz":3546893},"notes":notes,"destination":"out/notes"}),
    )
    .result;
    assert_eq!(rendered["notes"][0]["period"], 252);
    assert_eq!(rendered["chip"]["config"]["lfsr_width"], 15);
    assert_eq!(rendered["timing"]["tail_authority"], "default 0.5");
    let missing = service.call(
        "render_psg",
        json!({"job_id":"missing","notes":notes,"destination":"out/missing"}),
    );
    assert_eq!(missing.diagnostics[0].location, "/chip");
    let stereo = service.call(
        "render_psg",
        json!({"job_id":"stereo","chip":{"variant":"sn76489","clock_hz":3579545},
            "writes":[{"at_native_sample":0,"port":"stereo","data":240}],"destination":"out/stereo"}),
    );
    assert_eq!(stereo.diagnostics[0].location, "/writes/0/port");
    assert!(!root.join("out/missing").exists() && !root.join("out/stereo").exists());
}

#[test]
fn channel_selections_split_the_output_mix_without_changing_the_chip() {
    let root = workspace("channels");
    let mut service = Service::new(&root);
    // Without the Game Gear stereo write every channel reaches both sides,
    // so the SN76489 output is the plain sum of its four channels.
    let mut stream = body();
    stream.drain(16..18);
    let source = load(&mut service, &root, &vgm(&stream, None));
    let full = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"full","vgm":source,"destination":"out/full"}),
    );
    assert!(full.result.get("channel_selection").is_none());
    let whole = wav_frames(&root.join("out/full/render.wav"));
    let mut sum = vec![[0i32; 2]; whole.len()];
    for channel in 0..4 {
        let job = format!("solo-{channel}");
        let solo = ok(
            &mut service,
            "render_psg",
            json!({"job_id":job,"vgm":source,"channels":[{"solo":[channel]}],"destination":format!("out/{job}")}),
        );
        let report = &solo.result["channel_selection"]["chips"][0];
        assert_eq!(report["chip"], "sn76489");
        assert_eq!(report["heard"].as_array().unwrap().len(), 1);
        assert_eq!(report["muted"].as_array().unwrap().len(), 3);
        assert!(solo.summary.contains("left out of the output mix: sn76489"));
        assert_eq!(
            solo.result["output"]["final_state"],
            full.result["output"]["final_state"]
        );
        assert_eq!(
            solo.result["register_writes"],
            full.result["register_writes"]
        );
        for (total, frame) in sum
            .iter_mut()
            .zip(wav_frames(&root.join(format!("out/{job}/render.wav"))))
        {
            total[0] += i32::from(frame[0]);
            total[1] += i32::from(frame[1]);
        }
    }
    let expected: Vec<[i32; 2]> = whole
        .iter()
        .map(|f| [i32::from(f[0]), i32::from(f[1])])
        .collect();
    assert_eq!(sum, expected);
    // Explicit writes take the same selection by family name or without one.
    let tone = json!([{"at_native_sample":0,"data":0x8e},{"at_native_sample":0,"data":0x0f},
        {"at_native_sample":0,"data":0x90}]);
    let muted = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"muted","chip":{"variant":"sega","clock_hz":CLOCK},"writes":tone,
            "channels":[{"chip":"sn76489","mute":[0]}],"tail_seconds":0.01,"destination":"out/muted"}),
    );
    assert_eq!(muted.result["output"]["peak"], 0);
    for (job, channels, location) in [
        ("both", json!([{"solo":[0],"mute":[1]}]), "/channels/0"),
        ("neither", json!([{}]), "/channels/0"),
        ("empty", json!([{"mute":[]}]), "/channels/0/mute"),
        ("range", json!([{"mute":[4]}]), "/channels/0/mute/0"),
        (
            "unknown",
            json!([{"chip":"ay8910","mute":[0]}]),
            "/channels/0/chip",
        ),
        (
            "twice",
            json!([{"mute":[0]},{"chip":"sn76489","mute":[1]}]),
            "/channels/1/chip",
        ),
    ] {
        let refused = service.call(
            "render_psg",
            json!({"job_id":job,"vgm":source,"channels":channels,"destination":format!("out/{job}")}),
        );
        assert_eq!(refused.status, Status::Blocked, "{job}");
        assert_eq!(refused.diagnostics[0].location, location, "{job}");
        assert!(!root.join(format!("out/{job}")).exists());
    }
}
