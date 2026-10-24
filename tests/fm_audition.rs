//! A saved FM record auditions reproducibly with declared context, and refuses unknown context.
use serde_json::{Value, json};
use tunemend::service::{Service, Status, ToolResponse};

fn workspace(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "tunemend-fm-audition-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}
fn ok(service: &mut Service, tool: &str, args: Value) -> ToolResponse {
    let response = service.call(tool, args);
    assert_eq!(response.status, Status::Ok, "{response:?}");
    response
}
/// One 32-byte operator-row record at offset 0: algorithm 4, feedback 5,
/// operator 1 modulating operator 2 and operator 3 modulating operator 4.
fn container() -> Vec<u8> {
    let mut bytes = vec![0xee, 0xee, 0x2c, 0xee];
    // Rows are register slots 0..3, holding named operators 1, 3, 2, 4.
    for (tl, ar) in [(30u8, 31u8), (40, 31), (0, 31), (0, 31)] {
        bytes.extend([0x01, tl, ar, 0x05, 0x02, 0x27, 0x00]);
    }
    bytes
}
fn layout() -> Value {
    json!({"record_offsets":[0],"record_size":32,"algorithm_offset":2,"operators_offset":4,"packing":"operator_rows"})
}
fn context(chip: Option<&str>) -> Value {
    json!({"chip":chip,"clock_hz":7670454,"pan":"center"})
}

#[test]
fn bank_record_renders_with_reported_registers_and_replays() {
    let root = workspace("bank");
    std::fs::write(root.join("bank.bin"), container()).unwrap();
    let mut service = Service::new(&root);
    let bank = ok(
        &mut service,
        "load_fm_bank",
        json!({"job_id":"load","path":"bank.bin","chip":"ym2612","layout":layout()}),
    )
    .result["bank"]
        .clone();
    let request = json!({"job_id":"audition","bank":bank,"index":0,"context":context(None),
        "notes":[{"start_seconds":0.0,"key_on_seconds":0.25,"pitch":{"kind":"midi_note","note":69,"a4_hz":440.0}},
                 {"channel":1,"start_seconds":0.1,"key_on_seconds":0.2,"pitch":{"kind":"register","fnum":700,"block":4},"carrier_tl_offset":8}],
        "tail_seconds":1.0,"output_sample_rate":44100,"destination":"out/audition"});
    let first = ok(&mut service, "render_fm_patch", request.clone());
    let report = &first.result;
    assert_eq!(report["chip"], "ym2612");
    assert_eq!(report["chip_authority"], "bank declaration");
    assert_eq!(report["native_rate_exact"], "7670454/144");
    assert_eq!(report["wav_header_rate_hz"], 53267);
    assert_eq!(report["notes"][0]["fnum"], 1083);
    assert_eq!(report["notes"][0]["block"], 4);
    // Carriers in algorithm 4 are operators 2 and 4; the offset adds 8 TL steps.
    assert_eq!(report["notes"][1]["carrier_tl"][0]["tl"], 8);
    let writes = report["register_writes"].as_array().unwrap();
    let find = |port: u64, address: u64| {
        writes
            .iter()
            .filter(|w| w["port"] == port && w["address"] == address)
            .map(|w| w["data"].as_u64().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(find(0, 0xb0), [0x2c]);
    assert_eq!(find(0, 0xb4), [0xc0]);
    assert_eq!(find(0, 0x22), [0]);
    assert_eq!(find(0, 0x28), [0xf0, 0xf1, 0x00, 0x01]);
    // Named operator 2 lives in register slot 2 (offset 8): TL 0, then 8 for note 1.
    assert_eq!(find(0, 0x49), [0, 8]);
    assert!(
        report["context"]["defaults_applied"]
            .as_array()
            .unwrap()
            .contains(&json!("lfo=disabled"))
    );
    assert_eq!(report["source"]["raw_record"].as_array().unwrap().len(), 32);
    assert_eq!(report["hardware_audio"], false);

    let wav = std::fs::read(root.join("out/audition/audition.wav")).unwrap();
    assert_eq!(&wav[24..28], &53267u32.to_le_bytes());
    let frames = (wav.len() - 44) / 4;
    assert_eq!(frames as u64, report["frames"].as_u64().unwrap());
    let resampled = std::fs::read(root.join("out/audition/audition-44100.wav")).unwrap();
    assert_eq!(&resampled[24..28], &44100u32.to_le_bytes());
    assert_eq!(report["resampled"]["sample_rate"], 44100);
    let saved: Value =
        serde_json::from_slice(&std::fs::read(root.join("out/audition/audition.json")).unwrap())
            .unwrap();
    assert_eq!(saved["register_writes"], report["register_writes"]);
    let wav_artifact = first
        .artifacts
        .iter()
        .find(|a| a.kind == "audition.wav")
        .unwrap();
    assert_eq!(wav_artifact.sha256, tunemend::store::sha256_hex(&wav));

    let replay = service.call("render_fm_patch", request.clone());
    assert!(replay.replayed);
    assert_eq!(replay.result, first.result);
    // The same request in a new job and folder produces identical audio.
    let mut again = request;
    again["job_id"] = json!("again");
    again["destination"] = json!("out/again");
    ok(&mut service, "render_fm_patch", again);
    assert_eq!(
        std::fs::read(root.join("out/again/audition.wav")).unwrap(),
        wav
    );
}

#[test]
fn unknown_or_mismatched_context_refuses_without_changing_the_bank() {
    let root = workspace("refuse");
    std::fs::write(root.join("bank.bin"), container()).unwrap();
    let mut service = Service::new(&root);
    let family = ok(
        &mut service,
        "load_fm_bank",
        json!({"job_id":"family","path":"bank.bin","chip":"opn","layout":layout()}),
    )
    .result["bank"]
        .clone();
    let ym2203 = ok(
        &mut service,
        "load_fm_bank",
        json!({"job_id":"opn","path":"bank.bin","chip":"ym2203","layout":layout()}),
    )
    .result["bank"]
        .clone();
    let note = json!([{"start_seconds":0.0,"key_on_seconds":0.1,"pitch":{"kind":"register","fnum":1082,"block":4}}]);
    let render = |service: &mut Service, job: &str, bank: &Value, ctx: Value| {
        service.call(
            "render_fm_patch",
            json!({"job_id":job,"bank":bank,"index":0,"context":ctx,"notes":note,"destination":format!("out/{job}")}),
        )
    };
    // A family-only bank needs a declared chip; then it renders.
    let blocked = render(&mut service, "no-chip", &family, context(None));
    assert_eq!(blocked.status, Status::Blocked);
    assert!(!root.join("out/no-chip").exists());
    let declared = render(&mut service, "declared", &family, context(Some("ym2608")));
    assert_eq!(declared.status, Status::Ok, "{declared:?}");
    assert_eq!(declared.result["chip_authority"], "context declaration");
    // A declared chip must agree with the bank.
    let mismatch = render(&mut service, "mismatch", &ym2203, context(Some("ym2612")));
    assert_eq!(mismatch.status, Status::Blocked);
    // YM2203 has no LFO.
    let lfo = render(
        &mut service,
        "lfo",
        &ym2203,
        json!({"clock_hz":3993600,"pan":"center","lfo":{"enabled":true,"frequency":3}}),
    );
    assert_eq!(lfo.status, Status::Blocked);
    let mono = render(
        &mut service,
        "mono",
        &ym2203,
        json!({"clock_hz":3993600,"pan":"center"}),
    );
    assert_eq!(mono.status, Status::Ok, "{mono:?}");
    assert_eq!(mono.result["native_rate_exact"], "3993600/72");
    // Explicit registers need a chip and only interpreted bits.
    let explicit = json!({"algorithm_feedback":7,"operators":[[1,127,31,0,0,15,0],[1,127,31,0,0,15,0],[1,127,31,0,0,15,0],[1,0,31,0,0,15,0]]});
    let registers = |ctx: Value, job: &str| json!({"job_id":job,"registers":explicit,"context":ctx,"notes":note,"destination":format!("out/{job}")});
    assert_eq!(
        service
            .call("render_fm_patch", registers(context(None), "r1"))
            .status,
        Status::Blocked
    );
    assert_eq!(
        service
            .call("render_fm_patch", registers(context(Some("ym2612")), "r2"))
            .status,
        Status::Ok
    );
    // Original bank bytes are untouched.
    let export = ok(
        &mut service,
        "export_fm_bank",
        json!({"job_id":"export","bank":family,"destination":"out/bank"}),
    );
    assert_eq!(export.status, Status::Ok);
    assert_eq!(
        std::fs::read(root.join("out/bank/container.bin")).unwrap(),
        container()
    );
}
