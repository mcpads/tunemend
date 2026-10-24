//! Shared loading of the creative fixtures.
#![allow(dead_code)]

use tunemend::ProjectBundle;

pub fn fixture_text(name: &str) -> String {
    std::fs::read_to_string(format!("{}/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

pub fn village() -> ProjectBundle {
    serde_json::from_str(&fixture_text("village-loop.json")).unwrap()
}

pub fn chime() -> ProjectBundle {
    serde_json::from_str(&fixture_text("pickup-chime.json")).unwrap()
}
