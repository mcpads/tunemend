//! Data contract of Tunemend: the documents an agent reads and
//! writes, and the checks that decide whether they are consistent.

pub mod adx;
pub mod audio_clips;
pub mod ay;
pub mod bundle;
pub mod compare;
pub mod composition;
pub mod diagnostic;
pub mod edit;
pub mod effects;
pub mod export;
pub mod fm;
pub mod gb_apu;
pub mod huc6280;
pub mod md_dac;
pub mod measure;
pub mod midi;
pub mod mix;
pub mod nds_adpcm;
pub mod nds_sound;
pub mod nes_apu;
pub mod opn;
pub mod performance;
pub mod profile;
pub mod psg;
pub mod psx_adpcm;
pub mod psx_seq;
pub mod render;
pub mod review;
pub mod review_server;
pub mod sample_codec;
pub mod sample_loop;
pub mod sample_resample;
pub mod sample_wav;
pub mod service;
pub mod sound_chip;
pub mod soundfont;
pub mod spu_sequence;
pub mod spu_voice;
pub mod store;
pub mod synth;
pub mod target;
pub mod timeline;
pub mod vab;
pub mod vgm;
pub mod vgm_transform;
pub mod vgm_usage;
pub mod wav;
pub mod wonderswan;

pub use bundle::{ProjectBundle, check_project_bundle};
pub use diagnostic::{Diagnostic, DiagnosticCode};

/// JSON Schema of a project bundle, generated from the Rust types.
pub fn project_bundle_schema() -> schemars::Schema {
    schemars::schema_for!(ProjectBundle)
}

pub mod instruments;

pub mod compile_instruments;
pub mod instrument_source;
pub mod native_references;
pub mod snes_dsp;
pub mod voice_instruments;
