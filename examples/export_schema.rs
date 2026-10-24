//! Prints the JSON Schema of a project bundle generated from the Rust types.
//!
//! `cargo run --example export_schema > out/project-bundle.schema.json`

fn main() {
    let schema = tunemend::project_bundle_schema();
    println!(
        "{}",
        serde_json::to_string_pretty(&schema).expect("schema serializes")
    );
}
