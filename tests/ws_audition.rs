//! WonderSwan sound streams in original synthetic VGM files (0xBC port
//! writes and 0xC6 RAM writes) are interpreted, summarised per channel,
//! rendered like the equivalent explicit writes and mixed with other declared
//! chips; notes load wavetables into RAM; tails and the volume modifier work
//! as for the other chips.
use serde_json::{Value, json};
use tunemend::service::{Service, Status, ToolResponse};

const WS: u32 = 3_072_000;
const PCE: u32 = 3_579_545;

fn workspace(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("tunemend-ws-{name}-{}", std::process::id()));
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
fn vgm(ws: u32, pce: u32, volume_modifier: u8, body: &[u8], total: u32) -> Vec<u8> {
    let mut bytes = vec![0u8; 0x100];
    bytes[..4].copy_from_slice(b"Vgm ");
    bytes[8..12].copy_from_slice(&0x171u32.to_le_bytes());
    bytes[0x18..0x1c].copy_from_slice(&total.to_le_bytes());
    bytes[0x34..0x38].copy_from_slice(&(0x100u32 - 0x34).to_le_bytes());
    bytes[0x7c] = volume_modifier;
    bytes[0xa4..0xa8].copy_from_slice(&pce.to_le_bytes());
    bytes[0xc0..0xc4].copy_from_slice(&ws.to_le_bytes());
    bytes.extend(body);
    let len = bytes.len() as u32 - 4;
    bytes[4..8].copy_from_slice(&len.to_le_bytes());
    bytes
}

fn ports(writes: &[(u8, u8)]) -> Vec<u8> {
    writes.iter().flat_map(|&(r, d)| [0xbc, r, d]).collect()
}

fn ram(address: u16, bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .enumerate()
        .flat_map(|(i, &d)| {
            let [hi, lo] = (address + i as u16).to_be_bytes();
            [0xc6, hi, lo, d]
        })
        .collect()
}

/// A saw: samples 0..15 twice, two per byte (low nibble first).
fn saw() -> Vec<u8> {
    (0..16u8)
        .map(|k| ((2 * k) % 16) | (((2 * k + 1) % 16) << 4))
        .collect()
}

fn ws_body() -> Vec<u8> {
    let mut body = ports(&[(0x0f, 0x02), (0x11, 0x08)]);
    body.extend(ram(0x80, &saw()));
    body.extend(ram(0xa0, &saw()));
    body.extend(ports(&[
        (0x00, 0x26), // ch1 divisor 1830: 440 Hz
        (0x01, 0x07),
        (0x08, 0xff),
        (0x09, 0x80), // ch2 voice sample
        (0x14, 0x05),
        (0x04, 0xdc), // ch3 divisor 1500, sweeping up
        (0x05, 0x05),
        (0x0a, 0xaa),
        (0x0c, 0x01),
        (0x0d, 0x00),
        (0x06, 0x08), // ch4 divisor 1800, noise tap mode 3
        (0x07, 0x07),
        (0x0b, 0x88),
        (0x0e, 0x1b),
        (0x10, 0xed),
        (0x12, 0x55), // read-only: ignored
        (0x30, 0x00), // outside the sound ports
    ]));
    body.extend(ram(0x4000, &[0x11])); // beyond the wavetable reach
    body.extend(wait(2205));
    body.extend([0xbc, 0x90, 0x00]); // second chip without the dual bit
    body.extend(ports(&[(0x10, 0x00)]));
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
fn wonderswan_vgm_is_decoded_summarised_and_renders_like_its_writes() {
    let root = workspace("vgm");
    let mut service = Service::new(&root);
    let source = load(
        &mut service,
        &root,
        "ws.vgm",
        &vgm(WS, 0, 0, &ws_body(), 4410),
    );
    let summary = ok(&mut service, "inspect_vgm", json!({"source":source})).result;
    assert_eq!(summary["wonderswan"]["clock_hz"], WS);
    assert!(summary.get("huc6280").is_none());
    let kinds: Vec<&str> = summary["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"wonderswan_write"));
    assert!(kinds.contains(&"wonderswan_memory_write"));
    let usage = usage_of(&summary, "wonderswan");
    let keys: Vec<u64> = usage["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["key_ons"].as_u64().unwrap())
        .collect();
    assert_eq!(keys, [1, 1, 1, 1]);
    let ch1 = usage["channels"][0]["active_seconds"].as_f64().unwrap();
    assert!((ch1 - 0.05).abs() < 1e-9, "{ch1}");
    assert_eq!(usage["ports"]["internal_ram"], 33);
    assert_eq!(usage["ports"]["read_only_or_undefined"], 2);
    assert_eq!(usage["voice_mode_used"], true);
    assert_eq!(usage["sweep_used"], true);
    assert_eq!(usage["noise_mode_used"], true);
    assert_eq!(usage["unused_channels"], json!([]));
    assert_eq!(
        usage_of(&summary, "wonderswan_2")["ports"]["channel_control_90"],
        1
    );

    let commands = ok(
        &mut service,
        "inspect_vgm",
        json!({"source":source,"view":"commands","limit":500}),
    )
    .result;
    let items = commands["items"].as_array().unwrap();
    let memory: Vec<&Value> = items
        .iter()
        .filter(|i| i["kind"] == "wonderswan_memory_write")
        .collect();
    assert_eq!(memory[1]["meaning"]["wavetable"]["channel"], 1);
    assert_eq!(memory[1]["meaning"]["wavetable"]["values"], json!([2, 3]));
    assert_eq!(memory[16]["meaning"]["wavetable"]["channel"], 3);
    assert!(
        memory[32]["meaning"]["note"]
            .as_str()
            .unwrap()
            .contains("$3FFF")
    );
    let find = |name: &str| {
        items
            .iter()
            .find(|i| i["meaning"]["name"] == name)
            .unwrap()
            .clone()
    };
    assert_eq!(find("ch1_frequency_high")["meaning"]["divisor"], 1830);
    assert_eq!(find("ch4_noise_control")["meaning"]["sequence_length"], 217);
    assert_eq!(find("channel_control")["meaning"]["ch2_voice"], true);
    assert_eq!(find("ch2_voice_volume")["meaning"]["voice_left"], "100%");
    assert!(find("noise_lfsr_low_read")["meaning"]["ignored"].is_string());
    assert!(items.iter().any(|i| {
        i["meaning"]["ignored"]
            .as_str()
            .is_some_and(|s| s.contains("outside"))
    }));

    let rendered = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"ws","vgm":source,"destination":"out/ws"}),
    )
    .result;
    assert_eq!(rendered["chip"]["family"], "wonderswan");
    assert_eq!(rendered["timing"]["native_rate_exact"], format!("{WS}/128"));
    assert_eq!(rendered["skipped"].as_array().unwrap().len(), 1);
    assert_eq!(rendered["skipped"][0]["kind"], "wonderswan_write");
    let state = &rendered["output"]["final_state"];
    assert!(state["sweep_steps"].as_u64().unwrap() > 5);
    assert_eq!(state["memory_writes"], 33);
    let (rate, from_vgm) = wav_frames(&root.join("out/ws/render.wav"));
    assert_eq!(rate, 24_000);
    assert!(from_vgm.iter().any(|f| f[0] > 4000));
    assert!(from_vgm.iter().all(|f| f[0] >= 0 && f[1] >= 0));
    assert_eq!(from_vgm.len(), 2400);

    // The reported writes rendered directly give the same frames.
    let full: Value =
        serde_json::from_slice(&std::fs::read(root.join("out/ws/render.json")).unwrap()).unwrap();
    let writes: Vec<Value> = full["register_writes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| {
            let data = u8::from_str_radix(&w["data"].as_str().unwrap()[2..], 16).unwrap();
            if w["port"] == "memory" {
                json!({"at_tick":w["tick"],"address":w["address"],"data":data})
            } else {
                json!({"at_tick":w["tick"],"register":w["register"],"data":data})
            }
        })
        .collect();
    ok(
        &mut service,
        "render_psg",
        json!({"job_id":"direct","chip":{"variant":"wonderswan","clock_hz":WS},"writes":writes,
            "tail_seconds":0.05,"destination":"out/direct"}),
    );
    let (_, direct) = wav_frames(&root.join("out/direct/render.wav"));
    let n = direct.len().min(from_vgm.len());
    assert!(n > from_vgm.len() * 2 / 3);
    assert_eq!(&direct[..n], &from_vgm[..n]);
}

#[test]
fn wonderswan_mixes_with_the_huc6280_and_renders_dual_chips() {
    let root = workspace("mix");
    let mut service = Service::new(&root);
    let mut body = ram(0x00, &saw());
    body.extend(ports(&[
        (0x00, 0x26),
        (0x01, 0x07),
        (0x08, 0xff),
        (0x10, 0x01),
    ]));
    // The second chip plays channel 1 an octave up from its own (empty) RAM.
    body.extend([
        0xbc, 0x80, 0x93, 0xbc, 0x81, 0x03, 0xbc, 0x88, 0xff, 0xbc, 0x90, 0x01,
    ]);
    let pce = [(1u8, 0xffu8), (0, 0), (4, 0xdf), (5, 0xff), (6, 20)];
    body.extend(pce.iter().flat_map(|&(r, d)| [0xb9, r, d]));
    body.extend(wait(4410));
    body.push(0x66);
    let source = load(
        &mut service,
        &root,
        "mix.vgm",
        &vgm(WS | 0x4000_0000, PCE, 0, &body, 4410),
    );
    let summary = ok(&mut service, "inspect_vgm", json!({"source":source})).result;
    assert_eq!(summary["wonderswan"]["dual_chip"], true);
    assert_eq!(summary["huc6280"]["clock_hz"], PCE);
    refused(
        &mut service,
        json!({"job_id":"m0","vgm":source,"destination":"out/m0"}),
        "/output_sample_rate",
    );
    let mixed = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"m1","vgm":source,"output_sample_rate":48000,"include_native_wav":true,"destination":"out/m1"}),
    )
    .result;
    let ids: Vec<&str> = mixed["chips"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["huc6280", "wonderswan", "wonderswan_2"]);
    assert_eq!(mixed["mix"]["sources"], 3);
    assert!(root.join("out/m1/render-wonderswan_2.wav").is_file());
    assert_eq!(
        mixed["chips"][2]["output"]["final_state"]["channels"][0]["divisor"],
        0x393
    );
    assert_eq!(
        mixed["chips"][2]["output"]["final_state"]["memory_writes"],
        0
    );
    let only = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"m2","vgm":source,"families":["wonderswan"],"output_sample_rate":48000,"destination":"out/m2"}),
    )
    .result;
    assert_eq!(only["skipped"][0]["kind"], "huc6280_write");
    assert_eq!(
        only["skipped"][0]["render"],
        "not rendered: the family is not selected"
    );
}

#[test]
fn wonderswan_notes_load_wavetables_and_refuse_mismatches() {
    let root = workspace("notes");
    let mut service = Service::new(&root);
    let pitch = json!({"kind":"midi_note","note":69,"a4_hz":440.0});
    let wave: Vec<u8> = (0..32).map(|i| if i < 16 { 15 } else { 0 }).collect();
    let notes = json!([
        {"channel":0,"start_seconds":0.0,"duration_seconds":0.1,"pitch":pitch,"volume":15,"wavetable":wave},
        {"channel":2,"start_seconds":0.02,"duration_seconds":0.1,"pitch":{"kind":"period","period":1500},"volume":9,"wavetable":wave},
        {"channel":3,"start_seconds":0.05,"duration_seconds":0.05,"pitch":{"kind":"period","period":1900},"noise":{"taps":5},"volume":12}
    ]);
    let result = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"n","chip":{"variant":"wonderswan","clock_hz":WS},"notes":notes,"output_sample_rate":44100,"destination":"out/n"}),
    )
    .result;
    assert_eq!(result["notes"][0]["period"], 1830);
    assert_eq!(result["notes"][2]["noise"]["sequence_length"], 63);
    assert_eq!(result["setup_writes"][0]["name"], "wavetable_address");
    let state = &result["output"]["final_state"];
    assert_eq!(state["channels"][2]["wavetable"], json!(wave));
    assert_eq!(state["channel_control"], "0x00");
    let (rate, frames) = wav_frames(&root.join("out/n/render-44100.wav"));
    assert_eq!(rate, 44100);
    assert!(frames.iter().any(|f| f[0] > 3000));
    for (index, (mut request, location)) in [
        (json!({"chip":{"variant":"wonderswan","clock_hz":WS},"notes":[{"channel":0,"start_seconds":0.0,"duration_seconds":0.1,
            "pitch":pitch,"volume":15}]}), "/notes/0"),
        (json!({"chip":{"variant":"wonderswan","clock_hz":WS},"notes":[{"channel":3,"start_seconds":0.0,"duration_seconds":0.1,
            "pitch":pitch,"noise":{"taps":1},"volume":15}]}), "/notes/0"),
        (json!({"chip":{"variant":"wonderswan","clock_hz":WS},"notes":[{"channel":1,"start_seconds":0.0,"duration_seconds":0.1,
            "pitch":pitch,"volume":16,"wavetable":wave}]}), "/notes/0"),
        (json!({"chip":{"variant":"dmg","clock_hz":4_194_304},"notes":[{"channel":3,"start_seconds":0.0,"duration_seconds":0.1,
            "noise":{"mode":"white","divisor":1,"shift":3,"taps":1},"volume":15}]}), "/notes/0/noise/taps"),
        (json!({"chip":{"variant":"wonderswan","clock_hz":WS},"writes":[{"at_tick":0,"register":0x80,"data":1}]}), "/writes/0/register"),
        (json!({"chip":{"variant":"wonderswan","clock_hz":WS},"writes":[{"at_tick":0,"register":1,"address":64,"data":1}]}), "/writes/0"),
        (json!({"chip":{"variant":"wonderswan","clock_hz":WS},"writes":[{"at_tick":0,"data":1}]}), "/writes/0/register"),
        (json!({"chip":{"variant":"wonderswan","clock_hz":500_000},"notes":notes}), "/chip/clock_hz"),
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
fn wonderswan_vgm_tail_and_volume_modifier_apply_on_request() {
    let root = workspace("tail");
    let mut service = Service::new(&root);
    let mut body = ram(0x00, &saw());
    body.extend(ports(&[
        (0x00, 0x26),
        (0x01, 0x07),
        (0x08, 0x77),
        (0x10, 0x01),
    ]));
    body.extend(wait(4410));
    body.push(0x66);
    let source = load(
        &mut service,
        &root,
        "tail.vgm",
        &vgm(WS, 0, 0x20, &body, 4410),
    );
    // The tone still sounds at the end: the tail plays to its limit.
    let tailed = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"t1","vgm":source,"tail_seconds":0.25,"destination":"out/t1"}),
    )
    .result;
    assert_eq!(tailed["timing"]["tail"]["stopped_early"], false);
    assert_eq!(tailed["timing"]["tail"]["rendered_seconds"], 0.25);
    ok(
        &mut service,
        "render_psg",
        json!({"job_id":"t0","vgm":source,"destination":"out/t0"}),
    );
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
