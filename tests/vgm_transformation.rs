//! transform_vgm derives new retained VGM sources from original synthetic
//! files: same-family copies with declared changes for the SN76489, AY, Game
//! Boy and NES families, and the explicit SN76489 to AY mapping. The original
//! stays exact, every change is listed, losses refuse until accepted, the
//! result reads back through inspect_vgm and renders for comparison.
use serde_json::{Value, json};
use tunemend::service::{Service, Status, ToolResponse};

fn workspace(name: &str) -> std::path::PathBuf {
    let root =
        std::env::temp_dir().join(format!("tunemend-transform-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn call(service: &mut Service, tool: &str, args: Value, status: Status) -> ToolResponse {
    let response = service.call(tool, args);
    assert_eq!(response.status, status, "{response:?}");
    response
}

fn ok(service: &mut Service, tool: &str, args: Value) -> ToolResponse {
    call(service, tool, args, Status::Ok)
}

const SN: u32 = 3_579_545;
const MSX: u32 = 1_789_773;
const GB: u32 = 4_194_304;
const NES: u32 = 1_789_773;

#[derive(Clone, Copy)]
struct Clocks {
    sn: u32,
    ay: u32,
    gb: u32,
    nes: u32,
    /// VGM version; below 0x150 the header is 0x40 bytes.
    version: u32,
}

const DEFAULT: Clocks = Clocks {
    sn: 0,
    ay: 0,
    gb: 0,
    nes: 0,
    version: 0x171,
};

fn length(op: u8) -> (usize, u32) {
    match op {
        0x61 => (3, 0),
        0x62 => (1, 735),
        0x63 => (1, 882),
        0x70..=0x7f => (1, u32::from(op & 15) + 1),
        0x52 | 0x53 | 0xa0 | 0xb3 | 0xb4 => (3, 0),
        _ => (2, 0),
    }
}

/// An original synthetic VGM with a GD3 tag and an optional loop at a body offset.
fn vgm(clocks: Clocks, body: &[u8], loop_at: Option<usize>) -> Vec<u8> {
    let header_len = if clocks.version >= 0x150 { 0x100 } else { 0x40 };
    let mut bytes = vec![0u8; header_len];
    bytes[..4].copy_from_slice(b"Vgm ");
    bytes[8..12].copy_from_slice(&clocks.version.to_le_bytes());
    bytes[0x0c..0x10].copy_from_slice(&clocks.sn.to_le_bytes());
    if clocks.sn != 0 {
        bytes[0x28..0x2a].copy_from_slice(&9u16.to_le_bytes());
        bytes[0x2a] = 16;
    }
    if header_len == 0x100 {
        bytes[0x34..0x38].copy_from_slice(&(0x100u32 - 0x34).to_le_bytes());
        bytes[0x74..0x78].copy_from_slice(&clocks.ay.to_le_bytes());
        bytes[0x80..0x84].copy_from_slice(&clocks.gb.to_le_bytes());
        bytes[0x84..0x88].copy_from_slice(&clocks.nes.to_le_bytes());
    }
    let (mut i, mut total, mut looped) = (0, 0u32, 0u32);
    while body[i] != 0x66 {
        let (len, wait) = length(body[i]);
        let wait = if body[i] == 0x61 {
            u32::from(u16::from_le_bytes([body[i + 1], body[i + 2]]))
        } else {
            wait
        };
        total += wait;
        if loop_at.is_some_and(|l| i >= l) {
            looped += wait;
        }
        i += len;
    }
    bytes[0x18..0x1c].copy_from_slice(&total.to_le_bytes());
    if let Some(l) = loop_at {
        bytes[0x1c..0x20].copy_from_slice(&((header_len + l - 0x1c) as u32).to_le_bytes());
        bytes[0x20..0x24].copy_from_slice(&looped.to_le_bytes());
    }
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
    bytes
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

/// Every command of a source through inspect_vgm.
fn commands(service: &mut Service, source: &str) -> Vec<Value> {
    ok(
        service,
        "inspect_vgm",
        json!({"source":source,"view":"commands","limit":500}),
    )
    .result["items"]
        .as_array()
        .unwrap()
        .clone()
}

/// A Mega Drive-like stream: two SN76489 tones, noise, Game Gear stereo and YM2612 writes.
fn sn_body() -> Vec<u8> {
    vec![
        0x50, 0x8e, 0x50, 0x0f, 0x50, 0x92, // tone 0 period 0x0fe, attenuation 2
        0x52, 0x28, 0xf0, // YM2612 key-on (copied unchanged)
        0x61, 0x44, 0x01, // wait 324
        0x50, 0xe4, 0x50, 0xf3, // white noise rate 0, attenuation 3
        0x4f, 0xf1, // Game Gear stereo
        0x62, // wait 735 (loop start, body offset 18)
        0x50, 0xa0, 0x50, 0x08, 0x50, 0xb5, // tone 1 period 0x080, attenuation 5
        0x75, // wait 6
        0x53, 0x30, 0x01, 0x50, 0x9f, 0x63, // YM2612 port 1, tone 0 off, wait 882
        0x66,
    ]
}

#[test]
fn sn76489_copy_transposes_and_mutes_with_every_change_listed() {
    let root = workspace("sn");
    let mut service = Service::new(&root);
    let original = vgm(Clocks { sn: SN, ..DEFAULT }, &sn_body(), Some(18));
    let source = load(&mut service, &root, "song.vgm", &original);
    let response = ok(
        &mut service,
        "transform_vgm",
        json!({"job_id":"up","source":source,"copy":{"family":"sn76489","transpose_semitones":12,
            "transpose_channels":[0],"mute_channels":[3]},"destination":"out/up"}),
    );
    let result = &response.result;
    let derived = result["source"].as_str().unwrap().to_owned();
    assert_ne!(derived, source);
    assert_eq!(result["from"], source);
    assert_eq!(result["losses"], json!([]));
    assert_eq!(result["readback"]["other_commands_identical"], true);
    assert_eq!(result["readback"]["loop_at_same_sample"], true);
    assert_eq!(result["readback"]["loop_consistent_on_reentry"], true);
    // Only position bookkeeping changes in the header (the stream got shorter).
    assert!(
        result["header_changes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|h| h["reason"]
                .as_str()
                .unwrap()
                .starts_with("position bookkeeping"))
    );
    // The original is unchanged and exports exactly.
    ok(
        &mut service,
        "export_instrument_source",
        json!({"job_id":"export","source":source,"destination":"out/original"}),
    );
    assert_eq!(
        std::fs::read(root.join("out/original/original.bin")).unwrap(),
        original
    );
    // The file on disk is the retained derived source.
    let written = std::fs::read(root.join("out/up/transformed.vgm")).unwrap();
    assert_eq!(tunemend::store::sha256_hex(&written), derived);
    let listed = ok(
        &mut service,
        "inspect_instrument_source",
        json!({"view":"sources"}),
    )
    .result;
    let record = listed["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["source"] == derived)
        .unwrap()
        .clone();
    assert_eq!(record["derived_from"]["source"], source);
    assert_eq!(record["derived_from"]["tool"], "transform_vgm");

    // Readback through inspect_vgm: tone 0 is an octave up, noise muted, the rest equal.
    let before = commands(&mut service, &source);
    let after = commands(&mut service, &derived);
    assert_eq!(before.len(), after.len());
    assert_eq!(after[1]["meaning"]["register"], "tone0");
    assert_eq!(after[1]["meaning"]["after"], 0x7f);
    assert_eq!(
        after[6]["bytes"], "50 ff",
        "noise attenuation 3 rewritten to 15"
    );
    for i in [2, 3, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16] {
        assert_eq!(after[i]["bytes"], before[i]["bytes"], "command {i}");
        assert_eq!(after[i]["at_sample"], before[i]["at_sample"]);
    }
    let summary = ok(&mut service, "inspect_vgm", json!({"source":derived})).result;
    assert_eq!(summary["loop"]["consistent"], true);
    assert_eq!(summary["total_consistent"], true);
    assert_eq!(summary["gd3"]["strings"]["track_name"], "Synthetic");

    // Source and result render and compare.
    ok(
        &mut service,
        "render_psg",
        json!({"job_id":"r-a","vgm":source,"output_sample_rate":44100,"destination":"out/ra"}),
    );
    ok(
        &mut service,
        "render_psg",
        json!({"job_id":"r-b","vgm":derived,"output_sample_rate":44100,"destination":"out/rb"}),
    );
    let compared = ok(
        &mut service,
        "compare_audio",
        json!({"reference":{"path":"out/ra/render-44100.wav"},"candidate":{"path":"out/rb/render-44100.wav"}}),
    );
    assert!(
        compared.result.to_string().contains("residual"),
        "{}",
        compared.result
    );

    // A repeated job replays; a mute of a channel that never sounds changes no byte.
    let again = service.call(
        "transform_vgm",
        json!({"job_id":"up","source":source,"copy":{"family":"sn76489","transpose_semitones":12,
            "transpose_channels":[0],"mute_channels":[3]},"destination":"out/up"}),
    );
    assert!(again.replayed);
    let same = call(
        &mut service,
        "transform_vgm",
        json!({"job_id":"noop","source":source,"copy":{"family":"sn76489","mute_channels":[2]},"destination":"out/noop"}),
        Status::Blocked,
    );
    assert_eq!(
        same.diagnostics[0].code,
        tunemend::DiagnosticCode::EmptyEdit
    );
    assert!(!root.join("out/noop").exists());
}

#[test]
fn unrepresentable_results_refuse_until_the_loss_is_accepted() {
    let root = workspace("loss");
    let mut service = Service::new(&root);
    let source = load(
        &mut service,
        &root,
        "song.vgm",
        &vgm(Clocks { sn: SN, ..DEFAULT }, &sn_body(), None),
    );
    let refused = call(
        &mut service,
        "transform_vgm",
        json!({"job_id":"down","source":source,"copy":{"family":"sn76489","transpose_semitones":-48},"destination":"out/down"}),
        Status::Blocked,
    );
    assert_eq!(refused.diagnostics[0].location, "/accept_losses");
    assert_eq!(refused.result["unaccepted"], json!(["period_out_of_range"]));
    assert_eq!(refused.result["written"], false);
    assert!(!root.join("out/down").exists());
    let accepted = ok(
        &mut service,
        "transform_vgm",
        json!({"job_id":"down-2","source":source,"copy":{"family":"sn76489","transpose_semitones":-48},
            "accept_losses":["period_out_of_range"],"destination":"out/down"}),
    )
    .result;
    let derived = accepted["source"].as_str().unwrap().to_owned();
    let after = commands(&mut service, &derived);
    assert_eq!(
        after[1]["meaning"]["after"], 1023,
        "clamped to the largest period"
    );
    // A clock retarget that keeps pitch moves the noise rate, a named loss.
    let clock = call(
        &mut service,
        "transform_vgm",
        json!({"job_id":"pal","source":source,"copy":{"family":"sn76489","clock_hz":3546893},"destination":"out/pal"}),
        Status::Blocked,
    );
    assert_eq!(
        clock.result["unaccepted"],
        json!(["rate_scales_with_clock"])
    );
    // A loop whose first byte is a data byte reaches another register on the
    // second pass, so the rewritten loop cannot serve both passes.
    let latched = [
        0x50, 0x80, 0x50, 0x10, 0x50,
        0x90, // tone 0 period 0x100, attenuation 0 (latch on attenuation 0)
        0x61, 0x64, 0x00, // wait 100 (loop start, body offset 6)
        0x50, 0x08, // data byte: attenuation 0 on pass 1, tone 0 high bits on pass 2
        0x61, 0x64, 0x00, 0x50, 0x80, 0x61, 0x64, 0x00, 0x66,
    ];
    let looped = load(
        &mut service,
        &root,
        "latch.vgm",
        &vgm(Clocks { sn: SN, ..DEFAULT }, &latched, Some(6)),
    );
    let reentry = call(
        &mut service,
        "transform_vgm",
        json!({"job_id":"loop","source":looped,"copy":{"family":"sn76489","transpose_semitones":12},"destination":"out/loop"}),
        Status::Blocked,
    );
    assert_eq!(
        reentry.result["unaccepted"],
        json!(["loop_entry_state_differs"])
    );
    // Noise rate 3 follows tone 2: a transposed tone 2 moves the noise (a
    // loss), while a held tone 2 period keeps its value and moves nothing.
    for (tone2, expected) in [(0x100u16, Status::Blocked), (0, Status::Blocked)] {
        let body = [
            0x50,
            0xc0 | (tone2 & 15) as u8,
            0x50,
            (tone2 >> 4) as u8, // tone 2 period, attenuation stays 15
            0x50,
            0xe7, // white noise, rate 3 (tone 2)
            0x50,
            0xf0, // noise attenuation 0
            0x61,
            0x64,
            0x00,
            0x66,
        ];
        let name = format!("noise-{tone2}.vgm");
        let noisy = load(
            &mut service,
            &root,
            &name,
            &vgm(Clocks { sn: SN, ..DEFAULT }, &body, None),
        );
        let refused = call(
            &mut service,
            "transform_vgm",
            json!({"job_id":format!("noise-{tone2}"),"source":noisy,"copy":{"family":"sn76489","transpose_semitones":2},
                "destination":format!("out/noise-{tone2}")}),
            expected,
        );
        if tone2 == 0 {
            // Sega parts hold the output at period 0: no byte changes and no loss.
            assert_eq!(
                refused.diagnostics[0].code,
                tunemend::DiagnosticCode::EmptyEdit,
                "{refused:?}"
            );
        } else {
            assert_eq!(refused.result["unaccepted"], json!(["noise_follows_tone2"]));
        }
    }
    // Exactly one operation, and a family the header declares.
    call(
        &mut service,
        "transform_vgm",
        json!({"job_id":"none","source":source,"destination":"out/x"}),
        Status::Blocked,
    );
    let undeclared = call(
        &mut service,
        "transform_vgm",
        json!({"job_id":"ay","source":source,"copy":{"family":"ay8910","mute_channels":[0]},"destination":"out/x"}),
        Status::Blocked,
    );
    assert_eq!(undeclared.diagnostics[0].location, "/copy/family");
}

#[test]
fn ay_clock_retarget_keeps_pitch_and_variant_changes_the_header() {
    let root = workspace("ay");
    let mut service = Service::new(&root);
    let body = [
        0xa0, 0x00, 0xfd, 0xa0, 0x01, 0x01, // tone A period 0x1fd
        0xa0, 0x06, 0x10, 0xa0, 0x07, 0x3e, 0xa0, 0x08, 0x0f, // noise 16, tone A on, level 15
        0xa0, 0x0b, 0x00, 0xa0, 0x0c, 0x01, // envelope period 0x100
        0x61, 0x00, 0x10, // wait 4096
        0xa0, 0x00, 0x20, // fine only: period 0x120
        0x61, 0x00, 0x10, 0x66,
    ];
    let source = load(
        &mut service,
        &root,
        "ay.vgm",
        &vgm(Clocks { ay: MSX, ..DEFAULT }, &body, None),
    );
    let result = ok(
        &mut service,
        "transform_vgm",
        json!({"job_id":"st","source":source,"copy":{"family":"ay8910","clock_hz":2000000,"variant":"ym2149"},"destination":"out/st"}),
    )
    .result;
    let derived = result["source"].as_str().unwrap().to_owned();
    let fields: Vec<&str> = result["header_changes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|h| {
            !h["reason"]
                .as_str()
                .unwrap()
                .starts_with("position bookkeeping")
        })
        .map(|h| h["field"].as_str().unwrap())
        .collect();
    assert_eq!(fields, ["ay8910_clock", "ay8910_chip_type"]);
    let summary = ok(&mut service, "inspect_vgm", json!({"source":derived})).result;
    assert_eq!(summary["ay8910"]["variant"], "ym2149");
    assert_eq!(summary["ay8910"]["clock_hz"], 2_000_000);
    let after = commands(&mut service, &derived);
    let ratio = 2_000_000.0 / f64::from(MSX);
    let period = |p: f64| (p * ratio).round() as u64;
    assert_eq!(after[1]["meaning"]["tone_period"], period(509.0));
    assert_eq!(after[2]["meaning"]["noise_period"], period(16.0));
    assert_eq!(after[6]["meaning"]["envelope_period"], period(256.0));
    // The fine-only write needs the coarse part too: one inserted write.
    let late: Vec<&Value> = after.iter().filter(|c| c["at_sample"] == 4096).collect();
    assert_eq!(late.len(), 3, "fine, inserted coarse, end");
    assert_eq!(late[1]["meaning"]["tone_period"], period(288.0));
    assert_eq!(result["commands"]["original"], 11);
    assert_eq!(result["commands"]["output"], 12);
    // Both renders hold the same pitch: their spectra agree.
    ok(
        &mut service,
        "render_psg",
        json!({"job_id":"r-a","vgm":source,"output_sample_rate":44100,"destination":"out/ra"}),
    );
    ok(
        &mut service,
        "render_psg",
        json!({"job_id":"r-b","vgm":derived,"output_sample_rate":44100,"destination":"out/rb"}),
    );
    ok(
        &mut service,
        "compare_audio",
        json!({"reference":{"path":"out/ra/render-44100.wav"},"candidate":{"path":"out/rb/render-44100.wav"}}),
    );
}

#[test]
fn game_boy_and_nes_copies_mute_by_routing_and_report_retrigger_losses() {
    let root = workspace("gb-nes");
    let mut service = Service::new(&root);
    let gb_body = [
        0xb3, 0x16, 0x80, 0xb3, 0x14, 0x77, 0xb3, 0x15, 0xff, // power, volume, routing
        0xb3, 0x01, 0x80, 0xb3, 0x02, 0xf0, 0xb3, 0x03, 0xd6, 0xb3, 0x04,
        0x86, // pulse 1 x=0x6d6
        0x61, 0x00, 0x10, 0x66,
    ];
    let gb = load(
        &mut service,
        &root,
        "gb.vgm",
        &vgm(Clocks { gb: GB, ..DEFAULT }, &gb_body, None),
    );
    let result = ok(
        &mut service,
        "transform_vgm",
        json!({"job_id":"gb","source":gb,"copy":{"family":"gb_dmg","transpose_semitones":-12,"mute_channels":[1]},"destination":"out/gb"}),
    )
    .result;
    let after = commands(&mut service, result["source"].as_str().unwrap());
    assert_eq!(
        after[2]["bytes"], "b3 15 dd",
        "NR51 loses pulse 2 on both sides"
    );
    // 2048 - x doubles an octave down; the trigger and length bits of NR14 stay.
    assert_eq!(after[6]["meaning"]["period"], 2048 - 2 * (2048 - 0x6d6));
    assert_eq!(
        after[6]["meaning"]["data"],
        json!(0x80 | ((2048 - 2 * (2048 - 0x6d6)) >> 8))
    );

    let nes_body = [
        0xb4, 0x15, 0x0f, 0xb4, 0x00, 0xbf, 0xb4, 0x02, 0xfd, 0xb4, 0x03,
        0x08, // pulse 1 t=0x0fd
        0x61, 0x00, 0x10, 0xb4, 0x02, 0x10, // low only: t=0x010 (sweep-style update)
        0x61, 0x00, 0x10, 0x66,
    ];
    let nes = load(
        &mut service,
        &root,
        "nes.vgm",
        &vgm(
            Clocks {
                nes: NES,
                ..DEFAULT
            },
            &nes_body,
            None,
        ),
    );
    let refused = call(
        &mut service,
        "transform_vgm",
        json!({"job_id":"nes","source":nes,"copy":{"family":"nes_apu","transpose_semitones":-12},"destination":"out/nes"}),
        Status::Blocked,
    );
    assert_eq!(
        refused.result["unaccepted"],
        json!(["period_needs_retrigger"])
    );
    // PAL region with pitch kept: the clock changes and table rates are named.
    let pal = call(
        &mut service,
        "transform_vgm",
        json!({"job_id":"pal","source":nes,"copy":{"family":"nes_apu","variant":"nes_apu_pal"},"destination":"out/pal"}),
        Status::Blocked,
    );
    assert_eq!(
        pal.result["unaccepted"],
        json!(["frame_timing_scales_with_clock"])
    );
    let pal = ok(
        &mut service,
        "transform_vgm",
        json!({"job_id":"pal-2","source":nes,"copy":{"family":"nes_apu","variant":"nes_apu_pal"},
            "accept_losses":["frame_timing_scales_with_clock"],"destination":"out/pal"}),
    )
    .result;
    let summary = ok(&mut service, "inspect_vgm", json!({"source":pal["source"]})).result;
    assert_eq!(summary["nes_apu"]["region"], "pal");
    let muted = ok(
        &mut service,
        "transform_vgm",
        json!({"job_id":"nes-mute","source":nes,"copy":{"family":"nes_apu","mute_channels":[0]},"destination":"out/nes-mute"}),
    )
    .result;
    assert_eq!(
        commands(&mut service, muted["source"].as_str().unwrap())[0]["bytes"],
        "b4 15 0e"
    );
}

#[test]
fn sn76489_maps_to_ay_with_listed_losses_and_a_grown_header() {
    let root = workspace("map");
    let mut service = Service::new(&root);
    // VGM 1.10 with a 0x40-byte header: the mapping must reach 1.51.
    let original = vgm(
        Clocks {
            sn: SN,
            version: 0x110,
            ..DEFAULT
        },
        &sn_body(),
        Some(18),
    );
    let source = load(&mut service, &root, "song.vgm", &original);
    let mapping = json!({"variant":"ay8910","clock_hz":MSX});
    let refused = call(
        &mut service,
        "transform_vgm",
        json!({"job_id":"map","source":source,"map_sn76489_to_ay":mapping,"destination":"out/map"}),
        Status::Blocked,
    );
    let unaccepted: Vec<String> = refused.result["unaccepted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        unaccepted,
        [
            "volume_rounded",
            "noise_sequence_differs",
            "game_gear_stereo_dropped"
        ]
    );
    assert_eq!(refused.diagnostics.len(), 3);
    let result = ok(
        &mut service,
        "transform_vgm",
        json!({"job_id":"map-2","source":source,"map_sn76489_to_ay":mapping,
            "accept_losses":unaccepted,"destination":"out/map"}),
    )
    .result;
    let derived = result["source"].as_str().unwrap().to_owned();
    let fields: Vec<&str> = result["header_changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["field"].as_str().unwrap())
        .collect();
    for field in [
        "version",
        "sn76489_clock",
        "vgm_data_offset",
        "ay8910_clock",
        "gd3_offset",
        "loop_offset",
        "eof_offset",
    ] {
        assert!(fields.contains(&field), "{field} in {fields:?}");
    }
    let summary = ok(&mut service, "inspect_vgm", json!({"source":derived})).result;
    assert_eq!(summary["version"], "1.51");
    assert_eq!(summary["ay8910"]["variant"], "ay8910");
    assert!(
        summary["psg"]["render_refusal"].is_string(),
        "no SN76489 left"
    );
    assert_eq!(summary["loop"]["consistent"], true);
    assert_eq!(summary["total_consistent"], true);
    let after = commands(&mut service, &derived);
    let ay: Vec<&Value> = after
        .iter()
        .filter(|c| c["kind"] == "ay8910_write")
        .collect();
    // Tone 0 period 0xfe at 3579545 Hz is AY period 0xfe at 1789773 Hz.
    assert_eq!(ay[0]["meaning"]["tone_period"], 0xfe);
    assert!(
        after
            .iter()
            .all(|c| c["kind"] != "psg_write" && c["kind"] != "psg_stereo")
    );
    let ym: Vec<&Value> = after
        .iter()
        .filter(|c| c["kind"] == "other_chip_write")
        .collect();
    assert_eq!(ym.len(), 2, "YM2612 commands copied");
    assert_eq!(result["readback"]["other_commands_identical"], true);
    assert!(
        result["readback"]["register_checkpoints_matched"]
            .as_u64()
            .unwrap()
            > 0
    );
    // The AY render and the SN76489 render can be compared.
    ok(
        &mut service,
        "render_psg",
        json!({"job_id":"r-sn","vgm":source,"output_sample_rate":44100,"destination":"out/rsn"}),
    );
    let ay_render = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"r-ay","vgm":derived,"output_sample_rate":44100,"destination":"out/ray"}),
    )
    .result;
    assert_eq!(ay_render["chip"]["family"], "ay8910");
    ok(
        &mut service,
        "compare_audio",
        json!({"reference":{"path":"out/rsn/render-44100.wav"},"candidate":{"path":"out/ray/render-44100.wav"}}),
    );
    // Sharing the noise with a sounding tone is its own loss.
    let shared = call(
        &mut service,
        "transform_vgm",
        json!({"job_id":"shared","source":source,"map_sn76489_to_ay":{"variant":"ym2149","clock_hz":MSX,"noise_channel":0},
            "destination":"out/shared"}),
        Status::Blocked,
    );
    assert!(
        shared.result["unaccepted"]
            .as_array()
            .unwrap()
            .contains(&json!("noise_shares_tone_channel"))
    );
}

#[test]
fn a_named_result_file_names_the_written_vgm_and_the_new_source() {
    let root = workspace("named");
    let mut service = Service::new(&root);
    let original = vgm(Clocks { sn: SN, ..DEFAULT }, &sn_body(), Some(18));
    let source = load(&mut service, &root, "song.vgm", &original);
    let copy = json!({"family":"sn76489","transpose_semitones":2});
    let refused = call(
        &mut service,
        "transform_vgm",
        json!({"job_id":"bad","source":source,"copy":copy,"destination":"out/bad","vgm_name":"../up.vgm"}),
        Status::Blocked,
    );
    assert_eq!(refused.diagnostics[0].location, "/vgm_name");
    let response = ok(
        &mut service,
        "transform_vgm",
        json!({"job_id":"named","source":source,"copy":copy,"destination":"out/named","vgm_name":"song-up2.vgm"}),
    );
    assert!(root.join("out/named/song-up2.vgm").exists());
    assert!(!root.join("out/named/transformed.vgm").exists());
    let derived = response.result["source"].clone();
    let listed = ok(
        &mut service,
        "inspect_instrument_source",
        json!({"view":"sources"}),
    );
    let record = listed.result["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["source"] == derived)
        .unwrap()
        .clone();
    assert_eq!(record["name"], "song-up2.vgm");
    drop(service);
    std::fs::remove_dir_all(root).unwrap();
}
