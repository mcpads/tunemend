//! Original sample bytes and unrelated instruments survive cross-bank reuse.
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tunemend::instruments::{BankLayout, InstrumentBank, InstrumentSettings, SamplePlacement};
use tunemend::service::{Service, Status};

fn module(address: u16, body: &[u8]) -> Vec<u8> {
    [
        address.to_le_bytes().as_slice(),
        (body.len() as u16).to_le_bytes().as_slice(),
        body,
        &[255, 255],
    ]
    .concat()
}
fn layout() -> BankLayout {
    BankLayout {
        directory: 0x4500,
        slots: 55,
        tuning: 0x3af0,
        envelopes: 0x27d6,
        envelope_count: 23,
        sample_memory_end: 0xaae0,
        reserved_slots: vec![5],
    }
}
fn brr(blocks: usize, value: u8) -> Vec<u8> {
    let mut bytes = vec![value; blocks * 9];
    for i in 0..blocks {
        bytes[i * 9] = if i + 1 == blocks { 3 } else { 0 };
    }
    bytes
}
fn assets(donor: bool) -> BTreeMap<String, Vec<u8>> {
    let mut template = vec![0; 0x15e];
    template[0xf0 + 16 * 2..0xf2 + 16 * 2].copy_from_slice(&[27, 1]);
    let mut resident = vec![0; 0x1100];
    resident[0xfd6 + 22..0xfd8 + 22].copy_from_slice(&[255, 234]);
    let mut bank = vec![0; 700];
    let slot = if donor { 16 } else { 20 };
    let sample = brr(if donor { 2 } else { 4 }, if donor { 0x12 } else { 0x34 });
    bank[slot * 4..slot * 4 + 4].copy_from_slice(&[0, 0x46, if donor { 9 } else { 27 }, 0x46]);
    bank[0x100..0x100 + sample.len()].copy_from_slice(&sample);
    BTreeMap::from([
        ("template.bin".into(), module(0x3a00, &template)),
        ("bank.bin".into(), module(0x4500, &bank)),
        ("runtime-driver.bin".into(), module(0x0400, &[1, 2, 3])),
        ("runtime-data.bin".into(), module(0x1800, &resident)),
    ])
}
fn settings() -> InstrumentSettings {
    InstrumentSettings {
        srcn: 16,
        adsr: 11,
        octave_shift: -36,
        transpose: 0,
        vol_env: 0,
        vibrato: 0,
    }
}

#[test]
fn append_preserves_raw_brr_loop_tuning_and_every_other_byte() {
    let donor = InstrumentBank::new(layout(), assets(true)).unwrap();
    let target = InstrumentBank::new(layout(), assets(false)).unwrap();
    let (copied, report) = target
        .copy_from(&donor, &settings(), 20, SamplePlacement::Append)
        .unwrap();
    assert_eq!(report["method"], "append");
    assert_eq!(report["binding"]["srcn"], 20);
    assert_eq!(copied.sample(20).unwrap().bytes, brr(2, 0x12));
    assert_eq!(copied.sample(20).unwrap().loop_address, 0x4500 + 700 + 9);
    assert_eq!(copied.sample(20).unwrap().tuning, [27, 1]);
    assert_eq!(copied.envelope(11).unwrap(), [255, 234]);
    let before = &target.assets["bank.bin"];
    let after = &copied.assets["bank.bin"];
    assert_eq!(after.len(), before.len() + 18);
    for (i, byte) in before[..before.len() - 2].iter().enumerate() {
        if !(2..4).contains(&i) && !(4 + 20 * 4..4 + 20 * 4 + 4).contains(&i) {
            assert_eq!(*byte, after[i], "bank byte {i}");
        }
    }
    for (i, byte) in target.assets["template.bin"].iter().enumerate() {
        if !(4 + 0xf0 + 20 * 2..4 + 0xf2 + 20 * 2).contains(&i) {
            assert_eq!(*byte, copied.assets["template.bin"][i]);
        }
    }
    assert_eq!(target.sample(20).unwrap().bytes, brr(4, 0x34));
    // Existing byte-identical samples can be referenced again without growing the bank.
    let (reused, report) = copied
        .copy_from(&donor, &settings(), 22, SamplePlacement::Append)
        .unwrap();
    assert_eq!(report["method"], "reuse_identical");
    assert_eq!(reused.assets["bank.bin"].len(), after.len());
    assert_eq!(
        reused.sample(20).unwrap().bytes,
        reused.sample(22).unwrap().bytes
    );
}

#[test]
fn capacity_aliases_reserved_slots_and_runtime_mismatch_are_actionable() {
    let donor = InstrumentBank::new(layout(), assets(true)).unwrap();
    let mut tight = layout();
    tight.sample_memory_end = 0x4500 + 700;
    let target = InstrumentBank::new(tight.clone(), assets(false)).unwrap();
    assert!(
        target
            .copy_from(&donor, &settings(), 20, SamplePlacement::Append)
            .err()
            .unwrap()
            .contains("append capacity is 0")
    );
    let (replaced, report) = target
        .copy_from(&donor, &settings(), 20, SamplePlacement::Replace)
        .unwrap();
    assert_eq!(report["method"], "replace_exclusive");
    assert_eq!(
        replaced.assets["bank.bin"].len(),
        target.assets["bank.bin"].len()
    );
    assert!(
        target
            .copy_from(&donor, &settings(), 5, SamplePlacement::Replace)
            .err()
            .unwrap()
            .contains("reserved")
    );
    // A slot whose sample lives in a resident sample module is shared context:
    // replacement names the module boundary instead of a bare range error.
    let mut resident = assets(false);
    resident.insert("sample-0.bin".into(), module(0xb000, &brr(4, 0x56)));
    resident.get_mut("bank.bin").unwrap()[4 + 23 * 4..4 + 23 * 4 + 4]
        .copy_from_slice(&[0, 0xb0, 0, 0xb0]);
    let outside = InstrumentBank::new(tight.clone(), resident).unwrap();
    assert_eq!(outside.sample(23).unwrap().bytes, brr(4, 0x56));
    let refusal = outside
        .copy_from(&donor, &settings(), 23, SamplePlacement::Replace)
        .err()
        .unwrap();
    assert!(
        refusal.contains("outside the bank module") && refusal.contains("0xb000"),
        "{refusal}"
    );
    let mut shared = assets(false);
    shared.get_mut("bank.bin").unwrap()[4 + 21 * 4..4 + 21 * 4 + 4]
        .copy_from_slice(&[0, 0x46, 27, 0x46]);
    let target = InstrumentBank::new(tight, shared).unwrap();
    assert!(
        target
            .copy_from(&donor, &settings(), 20, SamplePlacement::Replace)
            .err()
            .unwrap()
            .contains("shared")
    );
    let mut other = assets(true);
    other.get_mut("runtime-driver.bin").unwrap()[4] = 42;
    let other = InstrumentBank::new(layout(), other).unwrap();
    assert!(
        target
            .copy_from(&other, &settings(), 20, SamplePlacement::Replace)
            .err()
            .unwrap()
            .contains("contracts differ")
    );
    let mut broken = assets(true);
    broken.get_mut("bank.bin").unwrap()[4 + 16 * 4 + 2..4 + 16 * 4 + 4].copy_from_slice(&[1, 0x46]);
    let broken = InstrumentBank::new(layout(), broken).unwrap();
    assert!(broken.sample(16).err().unwrap().contains("aligned block"));
    assert_eq!(broken.samples()[16]["available"], false);
    assert!(tunemend::instruments::module_blocks(&[0, 0x45, 9, 0, 1]).is_err());
}

fn workspace() -> PathBuf {
    let path = std::env::temp_dir().join(format!("tunemend-bank-service-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir(&path).unwrap();
    path
}
fn load(service: &mut Service, root: &Path, name: &str, donor: bool) -> Value {
    let dir = root.join(name);
    std::fs::create_dir(&dir).unwrap();
    for (file, bytes) in assets(donor) {
        std::fs::write(dir.join(file), bytes).unwrap();
    }
    let reply=service.call("load_instrument_bank",json!({"job_id":name,"template":dir.join("template.bin"),"bank":dir.join("bank.bin"),"driver":dir.join("runtime-driver.bin"),"resident":dir.join("runtime-data.bin"),"layout":layout()}));
    assert_eq!(reply.status, Status::Ok, "{reply:?}");
    reply.result["bank"].clone()
}
#[test]
fn copied_bank_resumes_and_supplies_the_actual_score_export_assets() {
    let root = workspace();
    let mut service = Service::new(&root);
    let donor = load(&mut service, &root, "donor", true);
    let target = load(&mut service, &root, "target", false);
    let request = json!({"job_id":"copy","source_bank":donor,"instrument":settings(),"destination_bank":target,"destination_slot":20});
    let copied = service.call("copy_instrument", request.clone());
    assert_eq!(copied.status, Status::Ok, "{copied:?}");
    assert_eq!(
        service.call("copy_instrument", request).result,
        copied.result
    );
    let bank = copied.result["bank"].clone();
    std::fs::remove_dir_all(root.join("donor")).unwrap();
    std::fs::remove_dir_all(root.join("target")).unwrap();
    drop(service);
    let mut service = Service::new(&root);
    let inspected = service.call(
        "inspect_instrument_bank",
        json!({"bank":bank,"view":"samples","offset":20,"limit":1}),
    );
    assert_eq!(inspected.result["items"][0]["bytes"], 18);
    std::fs::write(
        root.join("encoder.py"),
        include_str!("../fixtures/score-encoder-fixture.py"),
    )
    .unwrap();
    let midi =
        b"MThd\0\0\0\x06\0\0\0\x01\0\x30MTrk\0\0\0\x0c\0\x90\x3c\x64\x0c\x80\x3c\x40\0\xff\x2f\0";
    std::fs::write(root.join("source.mid"), midi).unwrap();
    let source = service.call(
        "load_midi",
        json!({"job_id":"load-midi","path":"source.mid"}),
    );
    assert_eq!(source.status, Status::Ok, "{source:?}");
    let plan=service.call("prepare_target",json!({"job_id":"score","source":source.result["source"],"target":{"encoder":"encoder.py","instrument_bank":bank,"python":if cfg!(windows){"python"}else{"python3"}},
        "mapping":{"ppqn":12,"whole_bars":false,"tracks":[{"name":"copied","channels":[0],"srcn":20,"adsr":11,"octave_shift":-36,"volume":24}]}}));
    assert_eq!(plan.status, Status::Ok, "{plan:?}");
    let changes: Vec<_> = plan.result["changes"].as_object().unwrap().keys().collect();
    let output=service.call("export_target",json!({"job_id":"out","plan":plan.result["plan"],"accepted_changes":changes,"destination":"out/transferred"}));
    assert_eq!(output.status, Status::Ok, "{output:?}");
    let b = std::fs::read(root.join("out/transferred/bank.bin")).unwrap();
    assert_eq!(&b[4 + 700..4 + 718], brr(2, 0x12));
    assert_eq!(
        std::fs::read(root.join("out/transferred/source.mid")).unwrap(),
        midi
    );
    assert!(root.join("out/transferred/runtime-driver.bin").is_file());
    let score = std::fs::read(root.join("out/transferred/score.bin")).unwrap();
    assert_eq!(&score[4 + 0xf0 + 40..4 + 0xf2 + 40], &[27, 1]);
    let report: Value =
        serde_json::from_slice(&std::fs::read(root.join("out/transferred/report.json")).unwrap())
            .unwrap();
    assert_eq!(report["instrument_bank"]["bank"], bank);
    let _ = std::fs::remove_dir_all(root);
}
