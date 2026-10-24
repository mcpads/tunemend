//! NES APU streams in original synthetic VGM files (0xB4 writes and 0xC2 DMC
//! memory blocks) are interpreted, rendered like the equivalent explicit
//! writes and mixed with other declared chips; VGM renders can add a tail and
//! apply the header volume modifier on request; the summary reports channel
//! usage.
use serde_json::{Value, json};
use tunemend::service::{Service, Status, ToolResponse};

const NES: u32 = 1_789_773;
const NES_PAL: u32 = 1_662_607;
const SN: u32 = 3_579_545;

fn workspace(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("tunemend-nes-{name}-{}", std::process::id()));
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

#[derive(Default)]
struct Header {
    sn: u32,
    nes: u32,
    volume_modifier: u8,
}

fn wait(samples: u16) -> Vec<u8> {
    let [lo, hi] = samples.to_le_bytes();
    vec![0x61, lo, hi]
}

/// An original synthetic VGM 1.71 file; `total` is the sum of its waits.
fn vgm(header: Header, body: &[u8], total: u32) -> Vec<u8> {
    let mut bytes = vec![0u8; 0x100];
    bytes[..4].copy_from_slice(b"Vgm ");
    bytes[8..12].copy_from_slice(&0x171u32.to_le_bytes());
    bytes[0x0c..0x10].copy_from_slice(&header.sn.to_le_bytes());
    if header.sn != 0 {
        bytes[0x28..0x2a].copy_from_slice(&9u16.to_le_bytes());
        bytes[0x2a] = 16;
    }
    bytes[0x18..0x1c].copy_from_slice(&total.to_le_bytes());
    bytes[0x34..0x38].copy_from_slice(&(0x100u32 - 0x34).to_le_bytes());
    bytes[0x7c] = header.volume_modifier;
    bytes[0x84..0x88].copy_from_slice(&header.nes.to_le_bytes());
    bytes.extend(body);
    let len = bytes.len() as u32 - 4;
    bytes[4..8].copy_from_slice(&len.to_le_bytes());
    bytes
}

fn nes(writes: &[(u8, u8)]) -> Vec<u8> {
    writes.iter().flat_map(|&(r, d)| [0xb4, r, d]).collect()
}

/// 17 DPCM bytes: rising, then falling.
fn sample() -> Vec<u8> {
    let mut data = vec![0xff; 9];
    data.extend([0x00; 8]);
    data
}

/// A data block of type 0xC2 writing `data` at `start`.
fn ram_write(start: u16, data: &[u8]) -> Vec<u8> {
    let mut block = vec![0x67, 0x66, 0xc2];
    block.extend((data.len() as u32 + 2).to_le_bytes());
    block.extend(start.to_le_bytes());
    block.extend(data);
    block
}

fn nes_body() -> Vec<u8> {
    let mut body = ram_write(0xc000, &sample());
    body.extend(nes(&[
        (0x10, 0x0f), // DMC rate $F, no loop
        (0x11, 0x20),
        (0x12, 0x00),
        (0x13, 0x01), // 17 bytes
        (0x17, 0x40),
        (0x00, 0xbf), // pulse 1: 50%, halt, constant 15
        (0x01, 0x08),
        (0x02, 0xfd),
        (0x03, 0x08),
        (0x08, 0xff), // triangle
        (0x0a, 0x7e),
        (0x0b, 0x08),
        (0x0c, 0x3a), // noise volume 10
        (0x0e, 0x04),
        (0x0f, 0x08),
        (0x15, 0x1f),
    ]));
    // The enable comes last, so the lengths loaded before it stay 0: load again.
    body.extend(nes(&[(0x03, 0x08), (0x0b, 0x08), (0x0f, 0x08)]));
    body.extend(wait(2205));
    body.extend(nes(&[
        (0x04, 0x7f),
        (0x05, 0x08),
        (0x06, 0x7c),
        (0x07, 0x09),
    ]));
    body.extend([0xb4, 0x20, 0x80]); // FDS register: not modelled
    body.extend([0xb4, 0x80, 0x00]); // second chip without the dual bit
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

#[test]
fn nes_vgm_is_decoded_and_renders_like_its_writes() {
    let root = workspace("vgm");
    let mut service = Service::new(&root);
    let source = load(
        &mut service,
        &root,
        "nes.vgm",
        &vgm(
            Header {
                nes: NES | 0x8000_0000,
                ..Default::default()
            },
            &nes_body(),
            4410,
        ),
    );
    let summary = ok(&mut service, "inspect_vgm", json!({"source":source})).result;
    assert_eq!(summary["nes_apu"]["clock_hz"], NES);
    assert_eq!(summary["nes_apu"]["region"], "ntsc");
    assert_eq!(summary["nes_apu"]["fds"], true);
    let kinds: Vec<String> = summary["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["kind"].as_str().unwrap().to_owned())
        .collect();
    assert!(kinds.contains(&"nes_apu_write".to_owned()));
    // Usage: every channel keyed once, the FDS write counted as a port.
    let usage = summary["usage"]["chips"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["chip"] == "nes_apu")
        .unwrap()
        .clone();
    let keys: Vec<u64> = usage["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["key_ons"].as_u64().unwrap())
        .collect();
    assert_eq!(keys, [1, 1, 1, 1, 1]);
    assert_eq!(usage["ports"]["fds_expansion"], 1);
    assert_eq!(usage["unused_channels"], json!([]));
    // Pulse 2 starts halfway through: about 0.05 s keyed.
    let pulse2 = usage["channels"][1]["active_seconds"].as_f64().unwrap();
    assert!((pulse2 - 0.05).abs() < 1e-9, "{pulse2}");
    // 17 bytes at 54 cycles per bit.
    let dmc = usage["channels"][4]["active_seconds"].as_f64().unwrap();
    assert!(
        (dmc - 17.0 * 8.0 * 54.0 / f64::from(NES)).abs() < 1e-4,
        "{dmc}"
    );
    assert_eq!(summary["usage"]["data_blocks"][0]["type"], "0xc2");

    let commands = ok(
        &mut service,
        "inspect_vgm",
        json!({"source":source,"view":"commands","limit":40}),
    )
    .result;
    let items = commands["items"].as_array().unwrap();
    let block = &items[0]["meaning"];
    assert_eq!(block["nes_apu_ram_write"]["start"], "$C000");
    assert_eq!(block["nes_apu_ram_write"]["bytes"], 17);
    assert!(items[0]["render"].as_str().unwrap().contains("DMC memory"));
    let address = items
        .iter()
        .find(|i| i["meaning"]["register"] == "dmc_sample_length")
        .unwrap();
    assert_eq!(address["meaning"]["sample_length_bytes"], 17);
    let pulse = items
        .iter()
        .find(|i| i["meaning"]["register"] == "pulse1_length_timer_high")
        .unwrap();
    // Before the $4015 enable the length load is refused by the chip.
    assert_eq!(pulse["meaning"]["length"]["loaded"], false);
    let fds = items
        .iter()
        .find(|i| i["meaning"]["expansion"] == "FDS")
        .unwrap();
    assert_eq!(fds["meaning"]["address"], "$4080");

    let rendered = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"nes","vgm":source,"destination":"out/nes"}),
    )
    .result;
    assert_eq!(rendered["chip"]["family"], "nes_apu");
    assert_eq!(rendered["timing"]["native_rate_exact"], format!("{NES}/16"));
    assert_eq!(rendered["expansion_audio"]["fds_declared"], true);
    let skipped: Vec<(String, String)> = rendered["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["kind"].as_str().unwrap().to_owned(),
                s["render"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].0, "nes_apu_write");
    let rows = rendered["skipped"][0]["count"].as_u64().unwrap();
    assert_eq!(rows, 2);
    let final_state = &rendered["output"]["final_state"];
    assert_eq!(final_state["dmc"]["bytes_read"], 17);
    assert_eq!(final_state["dmc"]["reads_of_unwritten_memory"], 0);
    let (rate, from_vgm) = wav_frames(&root.join("out/nes/render.wav"));
    assert_eq!(rate, 111_861);
    assert!(from_vgm.iter().all(|f| f[0] == f[1] && f[0] >= 0));

    // The reported writes and memory rendered directly give the same frames.
    let full: Value =
        serde_json::from_slice(&std::fs::read(root.join("out/nes/render.json")).unwrap()).unwrap();
    let mut writes = Vec::new();
    for w in full["register_writes"].as_array().unwrap() {
        if w["port"] == "dmc_memory" {
            assert_eq!(w["memory_block"]["start"], "$C000");
            continue;
        }
        writes.push(json!({"at_tick":w["tick"],"register":w["register"],
            "data":u8::from_str_radix(&w["data"].as_str().unwrap()[2..], 16).unwrap()}));
    }
    let hex: String = sample().iter().map(|b| format!("{b:02x}")).collect();
    ok(
        &mut service,
        "render_psg",
        json!({"job_id":"direct","chip":{"variant":"nes_apu","clock_hz":NES},"writes":writes,
            "dmc_memory":[{"address":0xc000,"data_hex":hex}],"tail_seconds":0.01,"destination":"out/direct"}),
    );
    let (_, direct) = wav_frames(&root.join("out/direct/render.wav"));
    let n = direct.len().min(from_vgm.len());
    assert!(n > from_vgm.len() / 2);
    assert_eq!(&direct[..n], &from_vgm[..n]);
}

#[test]
fn nes_mixes_with_other_chips_and_infers_pal_tables() {
    let root = workspace("mix");
    let mut service = Service::new(&root);
    let mut body = vec![0x50, 0x8e, 0x50, 0x0f, 0x50, 0x90];
    body.extend(nes(&[
        (0x15, 0x01),
        (0x00, 0xbf),
        (0x01, 0x08),
        (0x02, 0xfd),
        (0x03, 0x08),
    ]));
    body.extend(wait(4410));
    body.push(0x66);
    let source = load(
        &mut service,
        &root,
        "mix.vgm",
        &vgm(
            Header {
                sn: SN,
                nes: NES,
                ..Default::default()
            },
            &body,
            4410,
        ),
    );
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
    assert_eq!(ids, ["sn76489", "nes_apu"]);
    assert_eq!(mixed["mix"]["sources"], 2);
    assert!(root.join("out/m1/render-nes_apu.wav").is_file());
    let only = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"m2","vgm":source,"families":["nes_apu"],"destination":"out/m2"}),
    )
    .result;
    assert_eq!(only["chip"]["family"], "nes_apu");
    assert_eq!(
        only["skipped"][0]["render"],
        "not rendered: the family is not selected"
    );

    // A clock within 1% of the 2A07 clock selects the PAL tables.
    let pal = load(
        &mut service,
        &root,
        "pal.vgm",
        &vgm(
            Header {
                nes: NES_PAL,
                ..Default::default()
            },
            &[0x62, 0x66],
            735,
        ),
    );
    let summary = ok(&mut service, "inspect_vgm", json!({"source":pal})).result;
    assert_eq!(summary["nes_apu"]["region"], "pal");
    assert_eq!(summary["nes_apu"]["part"], "RP2A07");
}

#[test]
fn nes_notes_render_every_channel_and_refuse_mismatches() {
    let root = workspace("notes");
    let mut service = Service::new(&root);
    let pitch = json!({"kind":"midi_note","note":69,"a4_hz":440.0});
    let dpcm = json!({"data_hex":"ff".repeat(17),"rate":15,"start_level":32});
    let notes = json!([
        {"channel":0,"start_seconds":0.0,"duration_seconds":0.1,"pitch":pitch,"volume":12,"duty":1},
        {"channel":1,"start_seconds":0.02,"duration_seconds":0.1,"pitch":{"kind":"period","period":300},"volume":8},
        {"channel":2,"start_seconds":0.0,"duration_seconds":0.15,"pitch":pitch},
        {"channel":3,"start_seconds":0.05,"duration_seconds":0.05,"noise":{"mode":"periodic","period":6},"volume":9},
        {"channel":4,"start_seconds":0.1,"duration_seconds":0.05,"dpcm":dpcm}
    ]);
    let result = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"n","chip":{"variant":"nes_apu","clock_hz":NES},"notes":notes,"output_sample_rate":44100,"destination":"out/n"}),
    )
    .result;
    assert_eq!(result["notes"][0]["period"], 253);
    assert_eq!(result["notes"][2]["period"], 126);
    assert_eq!(result["notes"][4]["dpcm"]["address"], "$C000");
    assert_eq!(result["setup_writes"][0]["register"], "status");
    assert_eq!(result["setup_writes"][2]["dmc_memory"]["bytes"], 17);
    assert_eq!(result["output"]["final_state"]["dmc"]["bytes_read"], 17);
    let (rate, frames) = wav_frames(&root.join("out/n/render-44100.wav"));
    assert_eq!(rate, 44100);
    assert!(frames.iter().any(|f| f[0] != frames[0][0]));
    // PAL variant: its noise and DMC tables.
    let pal = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"p","chip":{"variant":"nes_apu_pal","clock_hz":NES_PAL},"notes":notes,"destination":"out/p"}),
    )
    .result;
    assert_eq!(pal["notes"][4]["dpcm"]["rate_cpu_cycles"], 50);
    assert_eq!(pal["chip"]["config"]["region"], "pal");
    for (index, (mut request, location)) in [
        (json!({"chip":{"variant":"sega","clock_hz":SN},"notes":[{"channel":0,"start_seconds":0.0,"duration_seconds":0.1,
            "pitch":pitch,"attenuation":0,"dpcm":dpcm}]}), "/notes/0/dpcm"),
        (json!({"chip":{"variant":"nes_apu","clock_hz":NES},"notes":[{"channel":4,"start_seconds":0.0,"duration_seconds":0.1,
            "dpcm":{"data_hex":"ffff","rate":1}}]}), "/notes/0"),
        (json!({"chip":{"variant":"nes_apu","clock_hz":NES},"notes":[{"channel":2,"start_seconds":0.0,"duration_seconds":0.1,
            "pitch":pitch,"volume":3}]}), "/notes/0"),
        (json!({"chip":{"variant":"nes_apu","clock_hz":NES},"writes":[{"at_tick":0,"register":0x20,"data":1}]}), "/writes/0/register"),
        (json!({"chip":{"variant":"nes_apu","clock_hz":NES},"notes":[{"channel":0,"start_seconds":0.0,"duration_seconds":0.1,
            "pitch":pitch,"volume":3}],"dmc_memory":[{"address":49152,"data_hex":"00"}]}), "/dmc_memory"),
        (json!({"chip":{"variant":"nes_apu","clock_hz":3_000_000},"notes":notes}), "/chip/clock_hz"),
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

/// SN76489 tone 0 at full level; `silence` ends it before the stream does.
fn sn_body(silence: bool) -> Vec<u8> {
    let mut body = vec![0x50, 0x8e, 0x50, 0x0f, 0x50, 0x90];
    body.extend(wait(2205));
    if silence {
        body.extend([0x50, 0x9f]);
        body.extend(wait(2205));
    }
    body.push(0x66);
    body
}

#[test]
fn vgm_tail_and_volume_modifier_are_explicit_and_reported() {
    let root = workspace("tail");
    let mut service = Service::new(&root);
    let header = || Header {
        sn: SN,
        volume_modifier: 0x20,
        ..Default::default()
    };
    let sounding = load(
        &mut service,
        &root,
        "sounding.vgm",
        &vgm(header(), &sn_body(false), 2205),
    );
    let plain = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"plain","vgm":sounding,"destination":"out/plain"}),
    )
    .result;
    assert!(plain["timing"].get("tail").is_none());
    assert_eq!(plain["not_applied"][0], "header volume modifier");
    let (_, base) = wav_frames(&root.join("out/plain/render.wav"));
    // A tone still sounding at the end plays through the whole tail.
    let tailed = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"tail","vgm":sounding,"tail_seconds":0.2,"destination":"out/tail"}),
    )
    .result;
    let tail = &tailed["timing"]["tail"];
    assert_eq!(tail["stopped_early"], false);
    assert!((tail["rendered_seconds"].as_f64().unwrap() - 0.2).abs() < 1e-9);
    let (_, longer) = wav_frames(&root.join("out/tail/render.wav"));
    assert_eq!(&longer[..base.len()], &base[..]);
    assert_eq!(
        longer.len(),
        (u64::from(SN) * (2205 + 8820) / (16 * 44100)) as usize
    );
    // A file that is silent at its end stops at once.
    let quiet = load(
        &mut service,
        &root,
        "quiet.vgm",
        &vgm(header(), &sn_body(true), 4410),
    );
    let response = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"quiet","vgm":quiet,"tail_seconds":1.0,"destination":"out/quiet"}),
    );
    let stopped = response.result;
    assert_eq!(stopped["timing"]["tail"]["stopped_early"], true);
    assert_eq!(stopped["timing"]["tail"]["rendered_seconds"], 0.0);
    // The constant run began before the stream end, and both the timing and
    // the summary say so.
    let constant_from = stopped["timing"]["tail"]["output_constant_from_seconds"]
        .as_f64()
        .unwrap();
    assert!(
        constant_from
            < stopped["timing"]["tail"]["stream_end_seconds"]
                .as_f64()
                .unwrap()
    );
    assert!(
        stopped["timing"]["tail"]["reason"]
            .as_str()
            .unwrap()
            .starts_with("no tail was needed")
    );
    assert!(
        response
            .summary
            .contains("tail 0.000 of 1.000 s rendered: no tail was needed"),
        "{}",
        response.summary
    );
    assert!(
        tail["reason"]
            .as_str()
            .unwrap()
            .starts_with("the whole tail was rendered")
    );
    refused(
        &mut service,
        json!({"job_id":"bad-tail","vgm":quiet,"tail_seconds":0.5,"range":{"start_sample":0,"end_sample":100},"destination":"out/bad"}),
        "/tail_seconds",
    );
    // The volume modifier 0x20 doubles every sample when requested.
    let louder = ok(
        &mut service,
        "render_psg",
        json!({"job_id":"vm","vgm":sounding,"apply_volume_modifier":true,"destination":"out/vm"}),
    )
    .result;
    assert_eq!(louder["volume_modifier"]["gain"], 2.0);
    assert_eq!(louder["volume_modifier"]["clipped_samples"], 0);
    assert_ne!(louder["not_applied"][0], "header volume modifier");
    let (_, doubled) = wav_frames(&root.join("out/vm/render.wav"));
    assert_eq!(doubled.len(), base.len());
    assert!(
        doubled
            .iter()
            .zip(&base)
            .all(|(d, b)| d[0] == 2 * b[0] && d[1] == 2 * b[1])
    );
}

#[test]
fn usage_reports_unused_channels_and_period_wrap_hints() {
    let root = workspace("usage");
    let mut service = Service::new(&root);
    // Tone 0 at period 0x3F0, then 0x010: 0x410 wrapped to 10 bits.
    let mut body = vec![0x50, 0x80, 0x50, 0x3f, 0x50, 0x90];
    body.extend(wait(1000));
    body.extend([0x50, 0x80, 0x50, 0x01]);
    body.extend(wait(1000));
    body.extend([0x50, 0xb4]); // tone 1 attenuation 4
    body.extend(wait(1000));
    body.push(0x66);
    let source = load(
        &mut service,
        &root,
        "wrap.vgm",
        &vgm(
            Header {
                sn: SN,
                ..Default::default()
            },
            &body,
            3000,
        ),
    );
    let usage = ok(&mut service, "inspect_vgm", json!({"source":source})).result["usage"].clone();
    let sn = &usage["chips"][0];
    assert_eq!(sn["chip"], "sn76489");
    assert_eq!(sn["unused_channels"], json!(["tone2", "noise"]));
    assert_eq!(sn["unused_ports"], json!(["game_gear_stereo"]));
    assert_eq!(sn["channels"][0]["key_ons"], 1);
    assert!(
        (sn["channels"][0]["active_seconds"].as_f64().unwrap() - 3000.0 / 44100.0).abs() < 1e-9
    );
    assert!(
        (sn["channels"][1]["active_seconds"].as_f64().unwrap() - 1000.0 / 44100.0).abs() < 1e-9
    );
    let hint = &usage["hints"][0];
    assert_eq!(hint["period_before"], 0x3f0);
    assert_eq!(hint["period"], 0x010);
}
