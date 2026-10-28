use serde_json::json;
use tunemend::service::{Service, Status, operation_group, surface_definitions, tool_definitions};

#[test]
fn every_operation_is_direct_or_discoverable_and_groups_refuse_cross_routing() {
    let root = std::env::temp_dir().join(format!("tunemend-routing-{}", std::process::id()));
    let mut service = Service::new(&root);
    let surface = surface_definitions();
    for operation in tool_definitions() {
        if let Some(group) = operation_group(operation.name) {
            assert!(surface.iter().any(|d| d.name == group));
            assert!(!surface.iter().any(|d| d.name == operation.name));
            let schema = service.call(
                group,
                json!({"operation":"describe","select":operation.name}),
            );
            assert_eq!(schema.status, Status::Ok);
            assert_eq!(
                schema.result["arguments_schema"],
                serde_json::to_value(operation.input_schema).unwrap()
            );
        } else {
            assert!(surface.iter().any(|d| d.name == operation.name));
        }
    }
    assert!(!root.exists(), "discovery must not create a workspace");
    let bad = service.call(
        "midi",
        json!({"operation":"export_sample","arguments":{"job_id":"bad"}}),
    );
    assert_eq!(bad.status, Status::Blocked);
    assert_eq!(bad.diagnostics[0].location, "/operation");
    assert!(!root.exists());
}

#[test]
fn routed_calls_keep_exact_job_replay_and_locate_invalid_arguments() {
    let root = std::env::temp_dir().join(format!("tunemend-route-replay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let mut service = Service::new(&root);
    let create = service.call(
        "create_project",
        json!({"job_id":"create","project":"test","timebase":{"kind":"music","ppq":480}}),
    );
    assert_eq!(create.status, Status::Ok, "{create:?}");
    let args = json!({"job_id":"brief","project":"test","brief":{"purpose":"quiet town"}});
    let direct = service.call("update_brief", args.clone());
    assert_eq!(direct.status, Status::Ok);
    let routed = service.call(
        "project",
        json!({"operation":"update_brief","arguments":args}),
    );
    assert!(routed.replayed);
    assert_eq!(routed.result, direct.result);
    assert_eq!(routed.job_id, direct.job_id);
    let bad = service.call(
        "project",
        json!({"operation":"update_brief","arguments":{"project":"test"}}),
    );
    assert_eq!(bad.status, Status::Blocked);
    assert_eq!(bad.diagnostics[0].location, "/arguments/job_id");
    drop(service);
    std::fs::remove_dir_all(root).unwrap();
}
