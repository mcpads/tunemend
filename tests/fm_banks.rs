//! Exact static patches survive changed storage order without rewriting opaque data.
use serde_json::{Value, json};
use tunemend::fm::{FmBank, FmLayout, OperatorPacking, OpnChip};
use tunemend::service::{Service, Status};

fn layout(columns: bool) -> FmLayout {
    FmLayout {
        record_offsets: vec![8, 40],
        record_size: 32,
        algorithm_offset: if columns { 28 } else { 2 },
        operators_offset: if columns { 0 } else { 4 },
        packing: if columns {
            OperatorPacking::RegisterColumns
        } else {
            OperatorPacking::OperatorRows
        },
        slot_order: if columns { [0, 2, 1, 3] } else { [0, 1, 2, 3] },
        reserved_records: vec![],
    }
}
fn source() -> FmBank {
    let mut bytes = vec![0x5a; 80];
    bytes[8..40].copy_from_slice(&[
        0xa1, 0xa2, 0x1d, 0xa3, 0x11, 0x22, 0x83, 0x94, 0x15, 0x67, 0x0e, 0x23, 0x34, 0xc5, 0x06,
        0x17, 0x89, 0x0a, 0x35, 0x46, 0x47, 0x18, 0x09, 0xab, 0x08, 0x47, 0x58, 0x09, 0x8a, 0x0b,
        0xcd, 0x0c,
    ]);
    FmBank::new(bytes, layout(false), OpnChip::Ym2612).unwrap()
}
#[test]
fn reordered_patch_preserves_all_other_bits_and_records() {
    let donor = source();
    let destination = FmBank::new(vec![0xff; 80], layout(true), OpnChip::Ym2608).unwrap();
    let (copied, report) = destination.copy_patch(&donor, 0, 0).unwrap();
    let mut expected = vec![0xff; 80];
    // Authored independently in stored order: named operators 1,2,3,4.
    expected[8..40].copy_from_slice(&[
        0x91, 0xb5, 0xa3, 0xc7, 0xa2, 0xc6, 0xb4, 0xd8, 0xa3, 0x67, 0xe5, 0x29, 0xf4, 0x78, 0x66,
        0xea, 0xf5, 0xe9, 0xf7, 0xeb, 0x67, 0xab, 0x89, 0xcd, 0xfe, 0xf8, 0xfa, 0xfc, 0xdd, 0xff,
        0xff, 0xff,
    ]);
    assert_eq!(copied.bytes, expected);
    assert_eq!(copied.patch(0).unwrap(), donor.patch(0).unwrap());
    assert_eq!(
        donor.inspect(0).unwrap()["decoded"]["operators"][0]["am_enabled"],
        true
    );
    assert_eq!(report["verification"]["unselected_bits_identical"], true);
    assert_eq!(destination.bytes, vec![0xff; 80]);
    let (restored, _) = donor.copy_patch(&copied, 0, 0).unwrap();
    assert_eq!(restored.bytes, donor.bytes);
}
#[test]
fn unsupported_hardware_settings_refuse_without_blocking_exact_loading() {
    let donor = source();
    let target = FmBank::new(vec![0xff; 80], layout(true), OpnChip::Ym2203).unwrap();
    assert!(
        target
            .copy_patch(&donor, 0, 0)
            .unwrap_err()
            .contains("AM enable")
    );
    // The old chip ignores AM bits. Loading keeps those raw bits and exposes them.
    let old = FmBank::new(donor.bytes.clone(), layout(false), OpnChip::Ym2203).unwrap();
    assert_eq!(old.bytes, donor.bytes);
    assert_eq!(old.patch(0).unwrap().operators[0][3], 0x14);
    assert!(
        !old.inspect(0).unwrap()["unmodeled_register_bits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let (copied, _) = target.copy_patch(&old, 0, 0).unwrap();
    assert_eq!(copied.patch(0).unwrap(), old.patch(0).unwrap());
}
#[test]
fn aliases_reservations_and_invalid_ranges_cannot_overwrite_other_data() {
    let donor = source();
    let mut l = layout(true);
    l.record_offsets = vec![8, 8];
    let alias = FmBank::new(vec![0; 80], l, OpnChip::Opn).unwrap();
    assert_eq!(alias.patch(0).unwrap(), alias.patch(1).unwrap());
    assert!(
        alias
            .copy_patch(&donor, 0, 0)
            .unwrap_err()
            .contains("aliases")
    );
    let mut reserved = source();
    reserved.layout.reserved_records.push(0);
    assert!(
        reserved
            .copy_patch(&donor, 0, 0)
            .unwrap_err()
            .contains("reserved")
    );
    assert!(FmBank::new(vec![0; 71], layout(false), OpnChip::Opn).is_err());
    let mut invalid = layout(false);
    invalid.operators_offset = usize::MAX;
    assert!(FmBank::new(vec![0; 80], invalid, OpnChip::Opn).is_err());
    let mut invalid = layout(false);
    invalid.algorithm_offset = 4;
    assert!(FmBank::new(vec![0; 80], invalid, OpnChip::Opn).is_err());
    let mut invalid = layout(false);
    invalid.slot_order = [0, 0, 2, 3];
    assert!(FmBank::new(vec![0; 80], invalid, OpnChip::Opn).is_err());
}
fn call(service: &mut Service, name: &str, args: Value) -> Value {
    let result = service.call(name, args);
    assert_eq!(result.status, Status::Ok, "{result:?}");
    result.result
}
#[test]
fn fm_workflow_replays_resumes_exports_exactly_and_refuses_corruption() {
    let root = std::env::temp_dir().join(format!("tunemend-fm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let donor = source();
    std::fs::write(root.join("original.bin"), &donor.bytes).unwrap();
    let mut service = Service::new(&root);
    let source = call(
        &mut service,
        "load_instrument_source",
        json!({"job_id":"raw","path":"original.bin"}),
    );
    let first = call(
        &mut service,
        "load_fm_bank",
        json!({"job_id":"first","source":source["source"],"chip":"ym2612","layout":donor.layout}),
    );
    std::fs::write(root.join("destination.bin"), vec![0xff; 80]).unwrap();
    let target = call(
        &mut service,
        "load_fm_bank",
        json!({"job_id":"target","path":"destination.bin","chip":"ym2608","layout":layout(true)}),
    );
    let request = json!({"job_id":"copy","source_bank":first["bank"],"source_index":0,"destination_bank":target["bank"],"destination_index":0});
    let copied = call(&mut service, "copy_fm_patch", request.clone());
    let replay = service.call("copy_fm_patch", request);
    assert!(replay.replayed);
    assert_eq!(replay.result, copied);
    std::fs::remove_file(root.join("original.bin")).unwrap();
    std::fs::remove_file(root.join("destination.bin")).unwrap();
    drop(service);
    let mut service = Service::new(&root);
    let banks = call(&mut service, "inspect_fm_bank", json!({"view":"banks"}));
    assert_eq!(banks["total"], 3);
    let page = call(
        &mut service,
        "inspect_fm_bank",
        json!({"bank":copied["bank"],"view":"records","limit":1}),
    );
    assert_eq!(page["next_cursor"], 1);
    assert_eq!(
        page["items"][0]["patch"],
        serde_json::to_value(donor.patch(0).unwrap()).unwrap()
    );
    call(
        &mut service,
        "export_fm_bank",
        json!({"job_id":"original-export","bank":first["bank"],"destination":"out/original"}),
    );
    assert_eq!(
        std::fs::read(root.join("out/original/container.bin")).unwrap(),
        donor.bytes
    );
    call(
        &mut service,
        "export_fm_bank",
        json!({"job_id":"export","bank":copied["bank"],"destination":"out/copy"}),
    );
    let exported = std::fs::read(root.join("out/copy/container.bin")).unwrap();
    assert_eq!(
        FmBank::new(exported, layout(true), OpnChip::Ym2608)
            .unwrap()
            .patch(0)
            .unwrap(),
        donor.patch(0).unwrap()
    );
    let overwrite = service.call(
        "export_fm_bank",
        json!({"job_id":"overwrite","bank":copied["bank"],"destination":"out/copy"}),
    );
    assert_eq!(overwrite.status, Status::Blocked);
    let raw_path = service
        .call(
            "load_instrument_source",
            json!({"job_id":"raw","path":"original.bin"}),
        )
        .artifacts[0]
        .path
        .clone();
    std::fs::write(raw_path, b"corrupt").unwrap();
    assert_eq!(
        service
            .call("inspect_fm_bank", json!({"bank":first["bank"]}))
            .status,
        Status::Error
    );
    let _ = std::fs::remove_dir_all(root);
}
