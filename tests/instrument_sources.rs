//! Source retention is independent of bank interpretation and native conversion.
use serde_json::json;
use tunemend::service::{Service, Status};

#[test]
fn opaque_sound_source_resumes_and_exports_exactly_without_a_bank() {
    let root = std::env::temp_dir().join(format!("tunemend-sound-source-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let original = b"An original opaque container\0\xff\x01\x03";
    std::fs::write(root.join("sound.bin"), original).unwrap();
    let mut service = Service::new(&root);
    let request = json!({"job_id":"load","path":"sound.bin"});
    let loaded = service.call("load_instrument_source", request.clone());
    assert_eq!(loaded.status, Status::Ok, "{loaded:?}");
    assert_eq!(loaded.result["discovery"]["catalogs"], json!([]));
    assert!(service.call("load_instrument_source", request).replayed);
    let id = loaded.result["source"].clone();
    std::fs::remove_file(root.join("sound.bin")).unwrap();
    drop(service);
    let mut service = Service::new(&root);
    let sources = service.call("inspect_instrument_source", json!({"view":"sources"}));
    assert_eq!(sources.result["sources"][0]["source"], id);
    let bytes = service.call(
        "inspect_instrument_source",
        json!({"source":id,"view":"bytes","offset":4,"limit":7}),
    );
    assert_eq!(bytes.result["bytes"], json!(&original[4..11]));
    let export = service.call(
        "export_instrument_source",
        json!({"job_id":"export","source":id,"destination":"out/original"}),
    );
    assert_eq!(export.status, Status::Ok, "{export:?}");
    assert_eq!(
        std::fs::read(root.join("out/original/original.bin")).unwrap(),
        original
    );
    let refuse = service.call(
        "export_instrument_source",
        json!({"job_id":"overwrite","source":id,"destination":"out/original"}),
    );
    assert_eq!(refuse.status, Status::Blocked);
    let original_path = &loaded.artifacts[0].path;
    std::fs::write(original_path, b"corrupt").unwrap();
    assert_eq!(
        service
            .call(
                "inspect_instrument_source",
                json!({"source":id,"view":"bytes"})
            )
            .status,
        Status::Error
    );
    let _ = std::fs::remove_dir_all(root);
}
