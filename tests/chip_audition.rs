//! AY-3-8910/YM2149 and Game Boy DMG streams in original synthetic VGM files
//! are interpreted, rendered like the equivalent explicit writes, mixed with
//! other declared chips under a reported policy, and refused when a declared
//! variant is not modelled.
use serde_json::{Value, json};
use tunemend::service::{Service, Status, ToolResponse};

fn workspace(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("tunemend-chips-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn ok(service: &mut Service, tool: &str, args: Value) -> ToolResponse {
    let response = service.call(tool, args);
    assert_eq!(response.status, Status::Ok, "{response:?}");
    response
}

const MSX: u32 = 1_789_773;
const GB: u32 = 4_194_304;
const SN: u32 = 3_579_545;

#[derive(Default)]
struct Header {
    sn: u32,
    ay: u32,
    ay_type: u8,
    ay_flags: u8,
    gb: u32,
}

fn command_length(op: u8) -> (usize, u32) {
    match op {
        0x61 => (3, 0),
        0x62 => (1, 735),
        0x63 => (1, 882),
        0x70..=0x7f => (1, u32::from(op & 15) + 1),
        0xa0 | 0xb3 | 0x52 => (3, 0),
        _ => (2, 0),
    }
}

/// An original synthetic VGM 1.71 file with a 0x100-byte header.
fn vgm(header: Header, body: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0u8; 0x100];
    bytes[..4].copy_from_slice(b"Vgm ");
    bytes[8..12].copy_from_slice(&0x171u32.to_le_bytes());
    bytes[0x0c..0x10].copy_from_slice(&header.sn.to_le_bytes());
    if header.sn != 0 {
        bytes[0x28..0x2a].copy_from_slice(&9u16.to_le_bytes());
        bytes[0x2a] = 16;
    }
    bytes[0x34..0x38].copy_from_slice(&(0x100u32 - 0x34).to_le_bytes());
    bytes[0x74..0x78].copy_from_slice(&header.ay.to_le_bytes());
    bytes[0x78] = header.ay_type;
    bytes[0x79] = header.ay_flags;
    bytes[0x80..0x84].copy_from_slice(&header.gb.to_le_bytes());
    let (mut i, mut total) = (0, 0u32);
    while body[i] != 0x66 {
        let (length, wait) = command_length(body[i]);
        total += if body[i] == 0x61 {
            u32::from(u16::from_le_bytes([body[i + 1], body[i + 2]]))
        } else {
            wait
        };
        i += length;
    }
    bytes[0x18..0x1c].copy_from_slice(&total.to_le_bytes());
    bytes.extend(body);
    let len = bytes.len() as u32 - 4;
    bytes[4..8].copy_from_slice(&len.to_le_bytes());
    bytes
}

fn wait(samples: u16) -> [u8; 3] {
    let [lo, hi] = samples.to_le_bytes();
    [0x61, lo, hi]
}

/// MSX-like AY stream: tone A, then noise on B with an envelope on C.
fn ay_body() -> Vec<u8> {
    let mut body = vec![
        0xa0, 0x00, 0xfd, 0xa0, 0x01, 0x01, // tone A period 0x1fd
        0xa0, 0x07, 0xb8, // tones on, noise off, port B output
        0xa0, 0x08, 0x0f, // A fixed level 15
        0xa0, 0x0e, 0x55, // I/O port A (not audible)
    ];
    body.extend(wait(2205));
    body.extend([
        0xa0, 0x06, 0x07, // noise period 7
        0xa0, 0x07, 0xae, // noise on B, tone B off
        0xa0, 0x09, 0x0c, // B level 12
        0xa0, 0x04, 0x80, 0xa0, 0x0b, 0x40, // tone C, envelope period 0x40
        0xa0, 0x0a, 0x10, 0xa0, 0x0d, 0x0e, // C follows the triangle envelope
        0x31, 0b00_10_01, // A left, B right, C off
        0x52, 0x28, 0xf0, // a YM2612 write (not modelled)
    ]);
    body.extend(wait(2205));
    body.push(0x66);
    body
}

/// Game Boy stream: power, mixer, a pulse note, wave RAM, a wave note, power off.
fn gb_body() -> Vec<u8> {
    let mut body = vec![
        0xb3, 0x16, 0x80, 0xb3, 0x14, 0x77, 0xb3, 0x15, 0xff, // power, volume, routing
        0xb3, 0x01, 0x80, 0xb3, 0x02, 0xf3, 0xb3, 0x03, 0xd6, 0xb3, 0x04,
        0x86, // CH1 50%, 0x6d6
    ];
    for i in 0..16u8 {
        body.extend([0xb3, 0x20 + i, (i << 4) | (15 - i)]);
    }
    body.extend(wait(1500));
    body.extend([
        0xb3, 0x0a, 0x80, 0xb3, 0x0c, 0x20, 0xb3, 0x0d, 0x00, 0xb3, 0x0e, 0x87,
    ]);
    body.extend(wait(1500));
    body.extend([0xb3, 0x16, 0x00, 0xb3, 0x12, 0xf0, 0xb3, 0x05, 0x01]); // off; ignored; unused
    body.extend(wait(500));
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
    let loaded = ok(
        service,
        "load_instrument_source",
        json!({"job_id":format!("load-{name}"),"path":name}),
    );
    loaded.result["source"].as_str().unwrap().to_owned()
}

/// The applied writes of a VGM render as explicit `at_tick` register writes.
fn as_explicit(report: &std::path::Path) -> Vec<Value> {
    let full: Value = serde_json::from_slice(&std::fs::read(report).unwrap()).unwrap();
    full["register_writes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| {
            let tick = w.get("tick").unwrap_or(&w["native_sample"]).clone();
            json!({"at_tick":tick,"register":w["register"],
                "data":u8::from_str_radix(&w["data"].as_str().unwrap()[2..], 16).unwrap()})
        })
        .collect()
}

#[test]
fn ay_vgm_is_decoded_and_renders_like_its_writes() {
    let root = workspace("ay");
    let mut service = Service::new(&root);
    let source = load(
        &mut service,
        &root,
        "msx.vgm",
        &vgm(
            Header {
                ay: MSX,
                ..Default::default()
            },
            &ay_body(),
        ),
    );
    let summary = ok(&mut service, "inspect_vgm", json!({"source":source})).result;
    assert_eq!(summary["ay8910"]["variant"], "ay8910");
    assert_eq!(summary["ay8910"]["clock_hz"], MSX);
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
        [
            "wait",
            "end",
            "other_chip_write",
            "ay8910_write",
            "ay8910_stereo_mask"
        ]
    );
    let commands = ok(
        &mut service,
        "inspect_vgm",
        json!({"source":source,"view":"commands","kind":"ay8910_write","limit":500}),
    )
    .result;
    let items = commands["items"].as_array().unwrap();
    // The coarse byte completes the 12-bit period written by the fine byte.
    assert_eq!(items[1]["meaning"]["name"], "tone_a_coarse");
    assert_eq!(items[1]["meaning"]["tone_period"], 0x1fd);
    assert_eq!(
        items[2]["meaning"]["tone_enabled"],
        json!([true, true, true])
    );
    assert_eq!(items[2]["meaning"]["io_b_output"], true);
    assert_eq!(items[4]["meaning"]["audible"], false);
    let shape = items
        .iter()
        .find(|i| i["meaning"]["name"] == "envelope_shape")
        .unwrap();
    assert_eq!(shape["meaning"]["shape"], "triangle starting with a rise");
    let header = ok(
        &mut service,
        "inspect_vgm",
        json!({"source":source,"view":"header"}),
    )
    .result;
    let chip_type = header["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "ay8910_chip_type")
        .unwrap()
        .clone();
    assert_eq!(chip_type["meaning"]["name"], "AY8910");

    let rendered = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"ay","vgm":source,"destination":"out/ay"}),
    )
    .result;
    assert_eq!(rendered["chip"]["family"], "ay8910");
    assert_eq!(rendered["timing"]["native_rate_exact"], format!("{MSX}/8"));
    assert_eq!(rendered["writes_applied"], 13);
    assert_eq!(rendered["skipped"][0]["kind"], "other_chip_write");
    assert!(
        rendered["model"]["renderer"]
            .as_str()
            .unwrap()
            .contains("AY-3-8910")
    );
    let writes = rendered["register_writes"].as_array().unwrap();
    let mask = writes.iter().find(|w| w["port"] == "stereo_mask").unwrap();
    assert_eq!(mask["effect"]["after"]["c"], "off");
    // 2205 VGM samples later: floor(2205 * clock / (8 * 44100)) ticks.
    assert_eq!(
        writes[5]["native_sample"],
        2205u64 * u64::from(MSX) / 352_800
    );
    let (rate, from_vgm) = wav_frames(&root.join("out/ay/render.wav"));
    assert_eq!(rate, 223_722);
    // Sides differ once the stereo mask applies.
    assert!(
        from_vgm[from_vgm.len() - 100..]
            .iter()
            .any(|f| f[0] != f[1])
    );

    // The reported writes rendered directly give the same frames (the stereo
    // mask is a VGM command, so compare the part before it).
    let explicit: Vec<Value> = as_explicit(&root.join("out/ay/render.json"))
        .into_iter()
        .filter(|w| !w["register"].is_null())
        .collect();
    ok(
        &mut service,
        "render_psg",
        json!({"job_id":"direct","chip":{"variant":"ay8910","clock_hz":MSX},"writes":explicit,"tail_seconds":0.0,"destination":"out/direct"}),
    );
    let (_, direct) = wav_frames(&root.join("out/direct/render.wav"));
    let mask_at = mask["native_sample"].as_u64().unwrap() as usize;
    assert_eq!(&direct[..mask_at], &from_vgm[..mask_at]);
    assert!(from_vgm[..mask_at].iter().all(|f| f[0] == f[1]));

    // A YM2149 with pin 26 low runs at half rate.
    let ym = load(
        &mut service,
        &root,
        "ym.vgm",
        &vgm(
            Header {
                ay: 4_000_000,
                ay_type: 0x10,
                ay_flags: 0x10,
                ..Default::default()
            },
            &ay_body(),
        ),
    );
    let ym_render = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"ym","vgm":ym,"destination":"out/ym"}),
    )
    .result;
    assert_eq!(ym_render["timing"]["native_rate_exact"], "4000000/16");
    assert_eq!(ym_render["chip"]["config"]["envelope_steps"], 32);
    // An AY8930 is declared but not modelled.
    let ay8930 = load(
        &mut service,
        &root,
        "8930.vgm",
        &vgm(
            Header {
                ay: MSX,
                ay_type: 3,
                ..Default::default()
            },
            &ay_body(),
        ),
    );
    let refused = service.call(
        "render_psg",
        json!({"job_id":"8930","vgm":ay8930,"destination":"out/8930"}),
    );
    assert_eq!(refused.diagnostics[0].location, "/vgm");
    assert!(refused.diagnostics[0].message.contains("AY8930"));
    assert!(!root.join("out/8930").exists());
}

#[test]
fn game_boy_vgm_renders_like_its_writes_and_reports_power_off() {
    let root = workspace("gb");
    let mut service = Service::new(&root);
    let source = load(
        &mut service,
        &root,
        "gb.vgm",
        &vgm(
            Header {
                gb: GB,
                ..Default::default()
            },
            &gb_body(),
        ),
    );
    let commands = ok(
        &mut service,
        "inspect_vgm",
        json!({"source":source,"view":"commands","kind":"gameboy_dmg_write","limit":500}),
    )
    .result;
    let items = commands["items"].as_array().unwrap();
    assert_eq!(items[3]["meaning"]["register"], "NR11");
    assert_eq!(items[3]["meaning"]["duty"], "50%");
    assert_eq!(items[6]["meaning"]["period"], 0x6d6);
    assert_eq!(items[6]["meaning"]["trigger"], true);
    assert_eq!(items[7]["meaning"]["register"], "wave_ram");
    let ignored = items.iter().find(|i| i["bytes"] == "b3 12 f0").unwrap();
    assert_eq!(
        ignored["meaning"]["ignored"],
        "APU off: register writes are ignored"
    );
    let unused = items.iter().find(|i| i["bytes"] == "b3 05 01").unwrap();
    assert!(
        unused["meaning"]["ignored"]
            .as_str()
            .unwrap()
            .contains("unused")
    );

    let rendered = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"gb","vgm":source,"output_sample_rate":44100,"include_native_wav":true,"destination":"out/gb"}),
    )
    .result;
    assert_eq!(rendered["chip"]["family"], "gb_dmg");
    assert_eq!(rendered["timing"]["native_rate_exact"], format!("{GB}/32"));
    assert_eq!(
        rendered["timing"]["vgm_to_tick"],
        "floor(vgm_sample * clock / (2 * 44100))"
    );
    assert_eq!(rendered["output"]["final_state"]["power"], false);
    let (rate, from_vgm) = wav_frames(&root.join("out/gb/render.wav"));
    assert_eq!(rate, 131_072);
    let power_off = (3000u64 * u64::from(GB) / (32 * 44100)) as usize;
    assert!(
        from_vgm[power_off + 1..].iter().all(|f| *f == [0, 0]),
        "silent after power-off"
    );
    assert!(from_vgm[..power_off].iter().any(|f| f[0] != 0));
    let explicit = as_explicit(&root.join("out/gb/render.json"));
    ok(
        &mut service,
        "render_psg",
        json!({"job_id":"direct","chip":{"variant":"dmg","clock_hz":GB},"writes":explicit,"tail_seconds":0.0,"destination":"out/direct"}),
    );
    let (_, direct) = wav_frames(&root.join("out/direct/render.wav"));
    assert_eq!(&direct[..power_off], &from_vgm[..power_off]);
}

#[test]
fn several_declared_chips_mix_at_the_output_rate() {
    let root = workspace("mix");
    let mut service = Service::new(&root);
    let mut body = vec![0x50, 0x8e, 0x50, 0x0f, 0x50, 0x90];
    body.extend(&ay_body()[..15]);
    body.extend(&gb_body()[..21]);
    body.extend(wait(4410));
    body.push(0x66);
    let source = load(
        &mut service,
        &root,
        "mix.vgm",
        &vgm(
            Header {
                sn: SN,
                ay: MSX,
                gb: GB,
                ..Default::default()
            },
            &body,
        ),
    );
    let refused = service.call(
        "render_psg",
        json!({"job_id":"no-rate","vgm":source,"destination":"out/no-rate"}),
    );
    assert_eq!(refused.diagnostics[0].location, "/output_sample_rate");
    assert!(!root.join("out/no-rate").exists());
    let mixed = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"mix","vgm":source,"output_sample_rate":48000,"include_native_wav":true,"destination":"out/mix"}),
    )
    .result;
    assert_eq!(mixed["mix"]["sources"], 3);
    assert_eq!(mixed["mix"]["gain_per_source"], 1.0 / 3.0);
    let ids: Vec<&str> = mixed["chips"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["sn76489", "ay8910", "gb_dmg"]);
    assert_eq!(mixed["writes_applied"], 3 + 5 + 7);
    assert!(mixed["register_writes"][3]["chip"] == "ay8910");
    for id in ids {
        assert!(root.join(format!("out/mix/render-{id}.wav")).exists());
    }
    let (rate, frames) = wav_frames(&root.join("out/mix/render-48000.wav"));
    assert_eq!((rate, frames.len()), (48000, 4800));
    assert_eq!(mixed["models"].as_object().unwrap().len(), 3);
    // Selecting one family renders it alone and reports the rest as skipped.
    let solo = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"solo","vgm":source,"families":["gb_dmg"],"destination":"out/solo"}),
    )
    .result;
    assert_eq!(solo["chip"]["family"], "gb_dmg");
    let reasons: Vec<&str> = solo["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["render"].as_str().unwrap())
        .collect();
    assert!(
        reasons.iter().all(|r| r.contains("not selected")),
        "{reasons:?}"
    );
    let ay_only = load(
        &mut service,
        &root,
        "ay-only.vgm",
        &vgm(
            Header {
                ay: MSX,
                ..Default::default()
            },
            &ay_body(),
        ),
    );
    let absent = service.call(
        "render_psg",
        json!({"job_id":"absent","vgm":ay_only,"families":["gb_dmg"],"destination":"out/absent"}),
    );
    assert_eq!(absent.diagnostics[0].location, "/families");

    // Channel selections name the chip when several render and keep every
    // other chip, the writes and the final state as without the selection.
    let unnamed = service.call(
        "render_psg",
        json!({"job_id":"unnamed","vgm":source,"output_sample_rate":48000,
            "channels":[{"mute":[0]}],"destination":"out/unnamed"}),
    );
    assert_eq!(unnamed.diagnostics[0].location, "/channels/0/chip");
    assert!(!root.join("out/unnamed").exists());
    let selected = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"selected","vgm":source,"output_sample_rate":48000,"include_native_wav":true,
            "channels":[{"chip":"ay8910","solo":[0]},{"chip":"gb_dmg","mute":[0,1,2,3]}],"destination":"out/selected"}),
    )
    .result;
    let chips = selected["channel_selection"]["chips"].as_array().unwrap();
    assert_eq!(chips[0]["chip"], "sn76489");
    assert_eq!(chips[0]["muted"], json!([]));
    assert_eq!(chips[1]["heard"], json!([{"channel":0,"name":"A"}]));
    assert_eq!(chips[2]["heard"], json!([]));
    assert_eq!(selected["mix"]["gain_per_source"], 1.0 / 3.0);
    for (a, b) in mixed["chips"]
        .as_array()
        .unwrap()
        .iter()
        .zip(selected["chips"].as_array().unwrap())
    {
        assert_eq!(a["output"]["final_state"], b["output"]["final_state"]);
        assert_eq!(a["writes_applied"], b["writes_applied"]);
    }
    let (_, sn_full) = wav_frames(&root.join("out/mix/render-sn76489.wav"));
    let (_, sn_selected) = wav_frames(&root.join("out/selected/render-sn76489.wav"));
    assert_eq!(sn_full, sn_selected);
    let (_, gb_selected) = wav_frames(&root.join("out/selected/render-gb_dmg.wav"));
    assert!(gb_selected.iter().all(|f| *f == [0, 0]));
    let wrong = service.call(
        "render_psg",
        json!({"job_id":"wrong","vgm":source,"output_sample_rate":48000,
            "channels":[{"chip":"ay8910","mute":[3]}],"destination":"out/wrong"}),
    );
    assert_eq!(wrong.diagnostics[0].location, "/channels/0/mute/0");
    assert!(wrong.diagnostics[0].message.contains("0 A, 1 B, 2 C"));
    let skipped = service.call(
        "render_psg",
        json!({"job_id":"skipped","vgm":source,"families":["gb_dmg"],
            "channels":[{"chip":"ay8910","mute":[0]}],"destination":"out/skipped"}),
    );
    assert_eq!(skipped.diagnostics[0].location, "/channels/0/chip");
}

#[test]
fn dual_chip_bits_select_the_second_instance() {
    let root = workspace("dual");
    let mut service = Service::new(&root);
    let mut body = vec![
        0xa0,
        0x87,
        0x0f,
        0xa0,
        0x80,
        0x40,
        0xa0,
        0x8a,
        0x0f,
        0x31,
        0x80 | 0b11_00_00,
    ];
    body.extend(wait(441));
    body.push(0x66);
    let dual = load(
        &mut service,
        &root,
        "dual.vgm",
        &vgm(
            Header {
                ay: MSX | 0x4000_0000,
                ..Default::default()
            },
            &body,
        ),
    );
    let rendered = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"dual","vgm":dual,"output_sample_rate":44100,"destination":"out/dual"}),
    )
    .result;
    let chips = rendered["chips"].as_array().unwrap();
    assert_eq!(chips[1]["id"], "ay8910_2");
    assert_eq!(chips[0]["writes_applied"], 0);
    assert_eq!(chips[1]["writes_applied"], 4);
    assert!(rendered["dual_chip"].is_string());
    let single = load(
        &mut service,
        &root,
        "single.vgm",
        &vgm(
            Header {
                ay: MSX,
                ..Default::default()
            },
            &body,
        ),
    );
    let rendered = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"single","vgm":single,"destination":"out/single"}),
    )
    .result;
    assert_eq!(rendered["writes_applied"], 0);
    let skipped = rendered["skipped"].as_array().unwrap();
    let writes = skipped
        .iter()
        .find(|s| s["kind"] == "ay8910_write")
        .unwrap();
    assert_eq!(writes["count"], 3);
    assert!(writes["render"].as_str().unwrap().contains("dual-chip"));
}

#[test]
fn explicit_notes_for_each_new_chip_and_located_refusals() {
    let root = workspace("notes");
    let mut service = Service::new(&root);
    let ay_notes = json!([
        {"channel":0,"start_seconds":0.0,"duration_seconds":0.05,"pitch":{"kind":"midi_note","note":69,"a4_hz":440.0},"volume":15},
        {"channel":1,"start_seconds":0.02,"duration_seconds":0.03,"noise":{"period":12},"volume":10}
    ]);
    let ay = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"ay","chip":{"variant":"ym2149","clock_hz":2_000_000},"notes":ay_notes,"tail_seconds":0.01,"destination":"out/ay"}),
    )
    .result;
    // clock / (16 * 440) = 284.09 -> period 284.
    assert_eq!(ay["notes"][0]["period"], 284);
    assert_eq!(ay["chip"]["config"]["variant"], "ym2149");
    let gb_notes = json!([
        {"channel":0,"start_seconds":0.0,"duration_seconds":0.1,"pitch":{"kind":"midi_note","note":69,"a4_hz":440.0},"volume":15,"duty":1,
            "envelope":{"direction":"down","pace":2}},
        {"channel":2,"start_seconds":0.0,"duration_seconds":0.1,"pitch":{"kind":"period","period":1798},
            "wave":{"samples":[0,2,4,6,8,10,12,14,15,13,11,9,7,5,3,1,0,2,4,6,8,10,12,14,15,13,11,9,7,5,3,1],"output_level":2}},
        {"channel":3,"start_seconds":0.05,"duration_seconds":0.05,"noise":{"mode":"periodic","divisor":3,"shift":4},"volume":12}
    ]);
    let gb = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"gb","chip":{"variant":"dmg","clock_hz":GB},"notes":gb_notes,"destination":"out/gb"}),
    )
    .result;
    // 2048 - 131072 / 440 = 1750.1 -> 1750.
    assert_eq!(gb["notes"][0]["period"], 1750);
    assert_eq!(gb["notes"][1]["output_level"], "50%");
    assert_eq!(gb["notes"][2]["noise"]["lfsr_width"], 7);
    assert_eq!(gb["setup_writes"][0]["register"], "NR52");
    let (rate, frames) = wav_frames(&root.join("out/gb/render.wav"));
    assert_eq!(rate, 131_072);
    assert!(frames.iter().any(|f| f[0] != frames[0][0]));
    for (index, (mut request, location)) in [
        (json!({"chip":{"variant":"ay8910","clock_hz":MSX},"notes":[{"channel":0,"start_seconds":0.0,"duration_seconds":0.1,
            "pitch":{"kind":"period","period":200},"attenuation":2}]}), "/notes/0"),
        (json!({"chip":{"variant":"ay8910","clock_hz":MSX},"writes":[{"at_tick":0,"register":16,"data":1}]}), "/writes/0/register"),
        (json!({"chip":{"variant":"dmg","clock_hz":GB},"writes":[{"at_tick":0,"data":1}]}), "/writes/0/register"),
        (json!({"chip":{"variant":"sega","clock_hz":SN},"writes":[{"at_tick":0,"register":1,"data":1}]}), "/writes/0"),
        (json!({"chip":{"variant":"dmg","clock_hz":GB},"notes":[{"channel":3,"start_seconds":0.0,"duration_seconds":0.1,
            "noise":{"mode":"white","rate":1},"volume":5}]}), "/notes/0"),
        (json!({"chip":{"variant":"ym2149","clock_hz":9_000_000},"notes":ay_notes}), "/chip/clock_hz"),
    ]
    .into_iter()
    .enumerate()
    {
        request["job_id"] = json!(format!("bad-{index}"));
        request["destination"] = json!("out/bad");
        let refused = service.call("render_psg", request);
        assert_eq!(refused.status, Status::Blocked, "{location}");
        assert_eq!(refused.diagnostics[0].location, location);
    }
    assert!(!root.join("out/bad").exists());
}
