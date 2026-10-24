//! HuC6280 PSG streams in original synthetic VGM files (0xB9 writes) are
//! interpreted, summarised per channel, rendered like the equivalent
//! explicit writes and mixed with other declared chips; notes load
//! wavetables; tails and the volume modifier work as for the other chips.
use serde_json::{Value, json};
use tunemend::service::{Service, Status, ToolResponse};

const PCE: u32 = 3_579_545;
const SN: u32 = 3_579_545;

fn workspace(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("tunemend-pce-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn ok(service: &mut Service, tool: &str, args: Value) -> ToolResponse {
    let response = service.call(tool, args);
    assert_eq!(response.status, Status::Ok, "{response:?}");
    response
}

fn refused(service: &mut Service, args: Value, location: &str) {
    let response = service.call("render_psg", args);
    assert_eq!(response.status, Status::Blocked, "{response:?}");
    assert_eq!(response.diagnostics[0].location, location);
}

fn wait(samples: u16) -> Vec<u8> {
    let [lo, hi] = samples.to_le_bytes();
    vec![0x61, lo, hi]
}

/// An original synthetic VGM 1.71 file; `total` is the sum of its waits.
fn vgm(sn: u32, pce: u32, volume_modifier: u8, body: &[u8], total: u32) -> Vec<u8> {
    let mut bytes = vec![0u8; 0x100];
    bytes[..4].copy_from_slice(b"Vgm ");
    bytes[8..12].copy_from_slice(&0x171u32.to_le_bytes());
    bytes[0x0c..0x10].copy_from_slice(&sn.to_le_bytes());
    if sn != 0 {
        bytes[0x28..0x2a].copy_from_slice(&9u16.to_le_bytes());
        bytes[0x2a] = 16;
    }
    bytes[0x18..0x1c].copy_from_slice(&total.to_le_bytes());
    bytes[0x34..0x38].copy_from_slice(&(0x100u32 - 0x34).to_le_bytes());
    bytes[0x7c] = volume_modifier;
    bytes[0xa4..0xa8].copy_from_slice(&pce.to_le_bytes());
    bytes.extend(body);
    let len = bytes.len() as u32 - 4;
    bytes[4..8].copy_from_slice(&len.to_le_bytes());
    bytes
}

fn pce(writes: &[(u8, u8)]) -> Vec<u8> {
    writes.iter().flat_map(|&(r, d)| [0xb9, r, d]).collect()
}

/// Select `channel`, load a saw from word 0, set divider and balance, turn it on.
fn saw_tone(channel: u8, divider: u16, al: u8) -> Vec<(u8, u8)> {
    let mut w = vec![(0, channel), (4, 0x40), (4, 0x00)];
    w.extend((0..32).map(|v| (6, v)));
    w.extend([
        (2, (divider & 0xff) as u8),
        (3, (divider >> 8) as u8),
        (5, 0xff),
        (4, 0x80 | al),
    ]);
    w
}

fn pce_body() -> Vec<u8> {
    let mut writes = vec![(1, 0xff)];
    writes.extend(saw_tone(0, 254, 31));
    // Channel 3 in direct D/A mode.
    writes.extend([
        (0, 2),
        (4, 0x40),
        (6, 0x00),
        (5, 0xff),
        (4, 0xdf),
        (6, 0x10),
    ]);
    // Channel 5 noise.
    writes.extend([(0, 4), (7, 0x9c), (5, 0xff), (4, 0x98)]);
    let mut body = pce(&writes);
    body.extend(wait(2205));
    // Channel 2 as the LFO source; channel 1 modulated with LF CTL 1.
    let mut lfo = saw_tone(1, 0x100, 0);
    lfo.extend([(8, 4), (9, 0x01), (0, 2), (6, 0x1f), (0x0a, 0x00)]);
    body.extend(pce(&lfo));
    body.extend([0xb9, 0x81, 0xff]); // second chip without the dual bit
    body.extend(wait(2205));
    body.extend(pce(&[(0, 0), (4, 0x00)]));
    body.extend(wait(2205));
    body.push(0x66);
    body
}

fn wav_frames(path: &std::path::Path) -> (u32, Vec<[i16; 2]>) {
    let bytes = std::fs::read(path).unwrap();
    let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
    let frames = bytes[44..]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| {
            [
                i16::from_le_bytes([c[0], c[1]]),
                i16::from_le_bytes([c[2], c[3]]),
            ]
        })
        .collect();
    (rate, frames)
}

fn load(service: &mut Service, root: &std::path::Path, name: &str, bytes: &[u8]) -> String {
    std::fs::write(root.join(name), bytes).unwrap();
    ok(
        service,
        "load_instrument_source",
        json!({"job_id":format!("load-{name}"),"path":name}),
    )
    .result["source"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn usage_of(summary: &Value, chip: &str) -> Value {
    summary["usage"]["chips"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["chip"] == chip)
        .unwrap()
        .clone()
}

#[test]
fn huc6280_vgm_is_decoded_summarised_and_renders_like_its_writes() {
    let root = workspace("vgm");
    let mut service = Service::new(&root);
    let source = load(
        &mut service,
        &root,
        "pce.vgm",
        &vgm(0, PCE, 0, &pce_body(), 6615),
    );
    let summary = ok(&mut service, "inspect_vgm", json!({"source":source})).result;
    assert_eq!(summary["huc6280"]["clock_hz"], PCE);
    assert_eq!(summary["huc6280"]["dual_chip"], false);
    assert!(summary.get("nes_apu").is_some_and(Value::is_null));
    let kinds: Vec<&str> = summary["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"huc6280_write"));
    let usage = usage_of(&summary, "huc6280");
    let keys: Vec<u64> = usage["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["key_ons"].as_u64().unwrap())
        .collect();
    // Channel 2 (AL 0) is written but never keyed; channel 1 sounds 0.1 s.
    assert_eq!(keys, [1, 0, 1, 0, 1, 0]);
    assert_eq!(usage["unused_channels"], json!(["ch4", "ch6"]));
    assert_eq!(usage["written_but_never_keyed"], json!(["ch2"]));
    let ch1 = usage["channels"][0]["active_seconds"].as_f64().unwrap();
    assert!((ch1 - 0.1).abs() < 1e-9, "{ch1}");
    assert_eq!(usage["waveform_writes"][0], 32);
    assert_eq!(usage["dda_writes"][2], 3);
    assert_eq!(usage["noise_enabled_channels"], json!(["ch5"]));
    assert_eq!(usage["lfo_enabled"], true);
    assert_eq!(usage["ports"]["undefined_0a_0f"], 1);
    // The second-chip write has its own usage entry.
    assert_eq!(
        usage_of(&summary, "huc6280_2")["ports"]["main_amplitude"],
        1
    );

    let commands = ok(
        &mut service,
        "inspect_vgm",
        json!({"source":source,"view":"commands","kind":"huc6280_write","limit":500}),
    )
    .result;
    let items = commands["items"].as_array().unwrap();
    assert_eq!(items[0]["meaning"]["register"], "main_amplitude");
    assert_eq!(items[0]["meaning"]["chip"], "HuC6280");
    let wave = items
        .iter()
        .filter(|i| i["meaning"]["waveform_write"].is_object())
        .collect::<Vec<_>>();
    assert_eq!(wave.len(), 64);
    assert_eq!(wave[5]["meaning"]["waveform_write"]["word_address"], 5);
    let frequency = items
        .iter()
        .find(|i| i["meaning"]["register"] == "frequency_high")
        .unwrap();
    assert_eq!(frequency["meaning"]["divider"], 254);
    assert_eq!(frequency["meaning"]["channel"], 1);
    let dda = items
        .iter()
        .find(|i| i["meaning"]["dda_level"] == 16)
        .unwrap();
    assert_eq!(dda["meaning"]["channel"], 3);
    let noise = items
        .iter()
        .find(|i| i["meaning"]["register"] == "noise")
        .unwrap();
    assert_eq!(noise["meaning"]["nf"], 3);
    let lfo = items
        .iter()
        .find(|i| i["meaning"]["register"] == "lfo_control")
        .unwrap();
    assert_eq!(lfo["meaning"]["lf_ctl"], 1);
    assert!(items.iter().any(|i| {
        i["meaning"]["ignored"]
            .as_str()
            .is_some_and(|s| s.contains("R0..R9"))
    }));

    let rendered = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"pce","vgm":source,"destination":"out/pce"}),
    )
    .result;
    assert_eq!(rendered["chip"]["family"], "huc6280");
    assert_eq!(rendered["timing"]["native_rate_exact"], format!("{PCE}/32"));
    assert_eq!(rendered["skipped"].as_array().unwrap().len(), 1);
    assert_eq!(rendered["skipped"][0]["kind"], "huc6280_write");
    assert!(
        rendered["skipped"][0]["render"]
            .as_str()
            .unwrap()
            .contains("dual-chip")
    );
    assert!(
        rendered["model"]["renderer"]
            .as_str()
            .unwrap()
            .contains("HuC6280")
    );
    assert_eq!(rendered["output"]["final_state"]["lfo"]["control"], "0x01");
    let (rate, from_vgm) = wav_frames(&root.join("out/pce/render.wav"));
    assert_eq!(rate, 111_861);
    // Left and right are equal (balance $FF everywhere) and something sounds.
    assert!(from_vgm.iter().all(|f| f[0] == f[1]));
    assert!(from_vgm.iter().any(|f| f[0] > 1000));

    // The reported writes rendered directly give the same frames.
    let full: Value =
        serde_json::from_slice(&std::fs::read(root.join("out/pce/render.json")).unwrap()).unwrap();
    let writes: Vec<Value> = full["register_writes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| {
            json!({"at_tick":w["tick"],"register":w["register"],
                "data":u8::from_str_radix(&w["data"].as_str().unwrap()[2..], 16).unwrap()})
        })
        .collect();
    ok(
        &mut service,
        "render_psg",
        json!({"job_id":"direct","chip":{"variant":"huc6280","clock_hz":PCE},"writes":writes,
            "tail_seconds":0.05,"destination":"out/direct"}),
    );
    let (_, direct) = wav_frames(&root.join("out/direct/render.wav"));
    let n = direct.len().min(from_vgm.len());
    assert!(n > from_vgm.len() * 2 / 3);
    assert_eq!(&direct[..n], &from_vgm[..n]);
}

#[test]
fn huc6280_mixes_with_other_chips_and_renders_dual_chips() {
    let root = workspace("mix");
    let mut service = Service::new(&root);
    let mut body = vec![0x50, 0x8e, 0x50, 0x0f, 0x50, 0x90];
    body.extend(pce(&[(1, 0xff)]));
    body.extend(pce(&saw_tone(0, 400, 31)));
    let second: Vec<(u8, u8)> = [(1, 0xff)]
        .into_iter()
        .chain(saw_tone(2, 300, 28))
        .map(|(r, d)| (r | 0x80, d))
        .collect();
    body.extend(pce(&second));
    body.extend(wait(4410));
    body.push(0x66);
    let source = load(
        &mut service,
        &root,
        "mix.vgm",
        &vgm(SN, PCE | 0x4000_0000, 0, &body, 4410),
    );
    let summary = ok(&mut service, "inspect_vgm", json!({"source":source})).result;
    assert_eq!(summary["huc6280"]["dual_chip"], true);
    assert_eq!(usage_of(&summary, "huc6280_2")["channels"][2]["key_ons"], 1);
    refused(
        &mut service,
        json!({"job_id":"m0","vgm":source,"destination":"out/m0"}),
        "/output_sample_rate",
    );
    let mixed = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"m1","vgm":source,"output_sample_rate":44100,"include_native_wav":true,"destination":"out/m1"}),
    )
    .result;
    let ids: Vec<&str> = mixed["chips"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["sn76489", "huc6280", "huc6280_2"]);
    assert_eq!(mixed["mix"]["sources"], 3);
    assert!(mixed["dual_chip"].as_str().unwrap().contains("second"));
    assert!(root.join("out/m1/render-huc6280_2.wav").is_file());
    assert_eq!(
        mixed["chips"][2]["output"]["final_state"]["channels"][2]["divider"],
        300
    );
    let only = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"m2","vgm":source,"families":["sn76489"],"destination":"out/m2"}),
    )
    .result;
    assert_eq!(
        only["skipped"][0]["render"],
        "not rendered: the family is not selected"
    );
    assert_eq!(only["skipped"][0]["kind"], "huc6280_write");
}

#[test]
fn huc6280_notes_load_wavetables_and_refuse_mismatches() {
    let root = workspace("notes");
    let mut service = Service::new(&root);
    let pitch = json!({"kind":"midi_note","note":69,"a4_hz":440.0});
    let square: Vec<u8> = (0..32).map(|i| if i < 16 { 31 } else { 0 }).collect();
    let notes = json!([
        {"channel":0,"start_seconds":0.0,"duration_seconds":0.1,"pitch":pitch,"volume":31,"wavetable":square},
        {"channel":3,"start_seconds":0.02,"duration_seconds":0.1,"pitch":{"kind":"period","period":300},"volume":25,"wavetable":square},
        {"channel":5,"start_seconds":0.05,"duration_seconds":0.05,"noise":{"period":28},"volume":20}
    ]);
    let result = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"n","chip":{"variant":"huc6280","clock_hz":PCE},"notes":notes,"output_sample_rate":44100,"destination":"out/n"}),
    )
    .result;
    assert_eq!(result["notes"][0]["period"], 254);
    assert_eq!(result["notes"][1]["attenuation_db"], 9.0);
    assert_eq!(result["notes"][2]["noise"]["nf"], 3);
    assert_eq!(result["setup_writes"][0]["register"], "main_amplitude");
    assert_eq!(
        result["output"]["final_state"]["channels"][0]["waveform"],
        json!(square)
    );
    let (rate, frames) = wav_frames(&root.join("out/n/render-44100.wav"));
    assert_eq!(rate, 44100);
    assert!(frames.iter().any(|f| f[0] > 1000));
    for (index, (mut request, location)) in [
        (json!({"chip":{"variant":"huc6280","clock_hz":PCE},"notes":[{"channel":0,"start_seconds":0.0,"duration_seconds":0.1,
            "pitch":pitch,"volume":31}]}), "/notes/0"),
        (json!({"chip":{"variant":"huc6280","clock_hz":PCE},"notes":[{"channel":2,"start_seconds":0.0,"duration_seconds":0.1,
            "noise":{"period":3},"volume":31}]}), "/notes/0"),
        (json!({"chip":{"variant":"nes_apu","clock_hz":1_789_773},"notes":[{"channel":0,"start_seconds":0.0,"duration_seconds":0.1,
            "pitch":pitch,"volume":3,"wavetable":square}]}), "/notes/0/wavetable"),
        (json!({"chip":{"variant":"huc6280","clock_hz":PCE},"notes":[{"channel":4,"start_seconds":0.0,"duration_seconds":0.1,
            "noise":{"period":3,"taps":1},"volume":31}]}), "/notes/0/noise/taps"),
        (json!({"chip":{"variant":"huc6280","clock_hz":PCE},"writes":[{"at_tick":0,"register":128,"data":1}]}), "/writes/0/register"),
        (json!({"chip":{"variant":"huc6280","clock_hz":PCE},"writes":[{"at_tick":0,"address":64,"data":1}]}), "/writes/0/address"),
        (json!({"chip":{"variant":"huc6280","clock_hz":9_000_000},"notes":notes}), "/chip/clock_hz"),
    ]
    .into_iter()
    .enumerate()
    {
        request["job_id"] = json!(format!("bad-{index}"));
        request["destination"] = json!("out/bad");
        refused(&mut service, request, location);
    }
    assert!(!root.join("out/bad").exists());
}

#[test]
fn huc6280_vgm_tail_and_volume_modifier_apply_on_request() {
    let root = workspace("tail");
    let mut service = Service::new(&root);
    let mut body = pce(&[(1, 0xff)]);
    body.extend(pce(&saw_tone(0, 254, 31)));
    body.extend(wait(4410));
    body.extend(pce(&[(0, 0), (4, 0x00)]));
    body.extend(wait(441));
    body.push(0x66);
    let source = load(
        &mut service,
        &root,
        "tail.vgm",
        &vgm(0, PCE, 0x20, &body, 4851),
    );
    let plain = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"t0","vgm":source,"destination":"out/t0"}),
    )
    .result;
    assert!(plain["timing"].get("tail").is_none());
    let tailed = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"t1","vgm":source,"tail_seconds":2.0,"destination":"out/t1"}),
    )
    .result;
    // The channel is off before the stream ends, so the tail stops at once.
    assert_eq!(tailed["timing"]["tail"]["stopped_early"], true);
    let (_, base) = wav_frames(&root.join("out/t0/render.wav"));
    let louder = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"t2","vgm":source,"apply_volume_modifier":true,"destination":"out/t2"}),
    )
    .result;
    assert_eq!(louder["volume_modifier"]["gain"], 2.0);
    let (_, doubled) = wav_frames(&root.join("out/t2/render.wav"));
    assert_eq!(base.len(), doubled.len());
    for (a, b) in base.iter().zip(&doubled) {
        assert_eq!(i32::from(b[0]), (i32::from(a[0]) * 2).clamp(-32768, 32767));
    }
}
