//! Workspace data that revisions refer to, carried with exported bundles:
//! sound sources of audio clips, and the instrument banks, FM banks and
//! sample plans of chip instruments, with every object they name.
use super::*;
use crate::bundle::ReferencedRecord;
use crate::voice_instruments::VoiceInstrument;
use audition::PlanRecord;
use instrument_sources::{SOURCES, SoundSourceRecord};
use std::collections::BTreeMap;

const PLANS: &str = "sample_plan";

/// Records an export carries and the object bytes they name, by SHA-256.
type CarriedData = (Vec<ReferencedRecord>, BTreeMap<String, Vec<u8>>);

/// A record some revision refers to, and where the first reference is.
pub(super) struct Need {
    pub collection: &'static str,
    pub id: String,
    /// JSON Pointer into the bundle.
    pub location: String,
    pub what: String,
}

/// Records the revisions refer to, each once, in revision order.
pub(super) fn needs(revisions: &[Revision]) -> Vec<Need> {
    let mut out: Vec<Need> = Vec::new();
    let mut push = |collection: &'static str, id: &str, location: String, what: String| {
        if !out.iter().any(|n| n.collection == collection && n.id == id) {
            out.push(Need {
                collection,
                id: id.to_owned(),
                location,
                what,
            });
        }
    };
    for (r, revision) in revisions.iter().enumerate() {
        let mix = &revision.composition.mix;
        let base = format!("/revisions/{r}/composition/mix");
        for (i, assignment) in mix.instruments.iter().enumerate() {
            let at = format!("{base}/instruments/{i}/instrument");
            let of = format!("voice `{}` in {}", assignment.voice, revision.id);
            match &assignment.instrument {
                VoiceInstrument::SnesBrr(s) => {
                    if let Some(bank) = &s.bank {
                        push(
                            instruments::BANKS,
                            bank,
                            format!("{at}/bank"),
                            format!("the instrument bank of {of}"),
                        );
                    }
                    if let Some(plan) = &s.plan {
                        push(
                            PLANS,
                            plan,
                            format!("{at}/plan"),
                            format!("the sample plan of {of}"),
                        );
                    }
                }
                VoiceInstrument::Fm(f) => {
                    if let Some(bank) = &f.bank {
                        push(
                            fm::FM_BANKS,
                            bank,
                            format!("{at}/bank"),
                            format!("the FM bank of {of}"),
                        );
                    }
                }
            }
        }
        for (i, clip) in mix.clips.iter().enumerate() {
            push(
                SOURCES,
                &clip.source,
                format!("{base}/clips/{i}/source"),
                format!("the sound source of clip `{}` in {}", clip.id, revision.id),
            );
        }
    }
    out
}

/// Checks a carried record's identity and returns the objects it names.
pub(super) fn carried_objects(reference: &ReferencedRecord) -> Result<Vec<String>, String> {
    fn typed<T: DeserializeOwned + Serialize>(record: &Value, id: &str) -> Result<T, String> {
        let typed: T = serde_json::from_value(record.clone()).map_err(|e| e.to_string())?;
        let bytes = serde_json::to_vec(&typed).expect("serializes");
        if sha256_hex(&bytes) != id {
            return Err("the record's SHA-256 differs from its id".into());
        }
        Ok(typed)
    }
    let (record, id) = (&reference.record, reference.id.as_str());
    Ok(match reference.collection.as_str() {
        SOURCES => {
            let source: SoundSourceRecord =
                serde_json::from_value(record.clone()).map_err(|e| e.to_string())?;
            if source.source != id {
                return Err("the record names another source".into());
            }
            vec![id.to_owned()]
        }
        instruments::BANKS => typed::<instruments::BankRecord>(record, id)?
            .assets
            .into_values()
            .collect(),
        fm::FM_BANKS => vec![typed::<fm::FmRecord>(record, id)?.asset],
        PLANS => typed::<PlanRecord>(record, id)?
            .files
            .into_values()
            .collect(),
        other => return Err(format!("records of `{other}` are not carried")),
    })
}

/// Canonical bytes of a hash-identified record, stored as an object like
/// the tools that create it do.
fn record_object(reference: &ReferencedRecord) -> Option<Vec<u8>> {
    let bytes = |value: Result<Vec<u8>, serde_json::Error>| value.ok();
    match reference.collection.as_str() {
        instruments::BANKS => bytes(
            serde_json::from_value::<instruments::BankRecord>(reference.record.clone())
                .and_then(|r| serde_json::to_vec(&r)),
        ),
        fm::FM_BANKS => bytes(
            serde_json::from_value::<fm::FmRecord>(reference.record.clone())
                .and_then(|r| serde_json::to_vec(&r)),
        ),
        PLANS => bytes(
            serde_json::from_value::<PlanRecord>(reference.record.clone())
                .and_then(|r| serde_json::to_vec(&r)),
        ),
        _ => None,
    }
}

/// Verified data an import adds to the workspace.
pub(super) struct Incoming {
    pub objects: Vec<Vec<u8>>,
    pub records: Vec<ReferencedRecord>,
}

impl Service {
    /// The record `need` names, verified in this workspace, and its objects.
    fn local_record(&mut self, need: &Need) -> Result<(Value, Vec<String>), ToolResponse> {
        let id = need.id.as_str();
        match need.collection {
            SOURCES => {
                self.sound_source_bytes(id)?;
                let record: SoundSourceRecord = self
                    .shared_document(SOURCES, id)?
                    .ok_or_else(|| ToolResponse::error("sound source vanished"))?;
                Ok((json!(record), vec![id.to_owned()]))
            }
            instruments::BANKS => {
                let (_, record) = self.instrument_bank_data(id)?;
                let objects = record.assets.values().cloned().collect();
                Ok((json!(record), objects))
            }
            fm::FM_BANKS => {
                let (_, record) = self.fm_bank_data(id)?;
                let objects = vec![record.asset.clone()];
                Ok((json!(record), objects))
            }
            _ => {
                let record: PlanRecord = self.shared_document(PLANS, id)?.ok_or_else(|| {
                    ToolResponse::refused(Code::UnknownReference, "/plan", "unknown sample plan")
                })?;
                if sha256_hex(&serde_json::to_vec(&record).expect("serializes")) != id {
                    return Err(ToolResponse::error("sample plan identity mismatch"));
                }
                let objects = record.files.values().cloned().collect();
                Ok((json!(record), objects))
            }
        }
    }

    /// Records and object bytes an export carries for `revisions`. Refuses
    /// when this workspace lacks something a revision refers to.
    pub(super) fn referenced_data(
        &mut self,
        revisions: &[Revision],
    ) -> Result<CarriedData, ToolResponse> {
        let mut records = Vec::new();
        let mut objects = BTreeMap::new();
        for need in needs(revisions) {
            let (record, names) = self.local_record(&need).map_err(|r| {
                ToolResponse::refused(
                    Code::UnknownReference,
                    &need.location,
                    format!(
                        "{} ({} {}) cannot be read in this workspace: {}",
                        need.what, need.collection, need.id, r.summary
                    ),
                )
            })?;
            let store = self.store_ref()?;
            for name in names {
                let bytes = std::fs::read(store.object_path(&name))
                    .map_err(|e| ToolResponse::error(format!("object {name}: {e}")))?;
                if sha256_hex(&bytes) != name {
                    return Err(ToolResponse::error(format!(
                        "object {name} identity mismatch"
                    )));
                }
                objects.insert(name, bytes);
            }
            records.push(ReferencedRecord {
                collection: need.collection.to_owned(),
                id: need.id,
                record,
            });
        }
        Ok((records, objects))
    }

    /// Verifies the records and objects a bundle carries (objects from
    /// `folder`, or already in this workspace) and checks that every record
    /// a revision needs is carried or retained here. Nothing is written.
    pub(super) fn incoming_data(
        &mut self,
        bundle: &ProjectBundle,
        folder: &Path,
    ) -> Result<Incoming, ToolResponse> {
        let mut incoming = Incoming {
            objects: Vec::new(),
            records: Vec::new(),
        };
        let have_store = Store::exists(&self.workspace);
        for (i, reference) in bundle.references.iter().enumerate() {
            let names = carried_objects(reference).map_err(|e| {
                ToolResponse::refused(
                    Code::RevisionContentMismatch,
                    &format!("/references/{i}"),
                    format!("{} `{}`: {e}", reference.collection, reference.id),
                )
            })?;
            for name in names {
                let file = folder.join(&name);
                match std::fs::read(&file) {
                    Ok(bytes) if sha256_hex(&bytes) == name => incoming.objects.push(bytes),
                    Ok(_) => {
                        return Err(ToolResponse::refused(
                            Code::RevisionContentMismatch,
                            "/path",
                            format!("{} differs from its SHA-256 name", file.display()),
                        ));
                    }
                    Err(_) => {
                        let retained = have_store
                            && self.store_ref().is_ok_and(|store| {
                                std::fs::read(store.object_path(&name))
                                    .is_ok_and(|b| sha256_hex(&b) == name)
                            });
                        if !retained {
                            return Err(ToolResponse::refused(
                                Code::UnknownReference,
                                "/path",
                                format!(
                                    "object {name} of {} `{}` is missing from {} and not retained in this workspace; export with referenced_data=include, or load that data here first",
                                    reference.collection,
                                    reference.id,
                                    folder.display()
                                ),
                            ));
                        }
                    }
                }
            }
            if let Some(bytes) = record_object(reference) {
                incoming.objects.push(bytes);
            }
            incoming.records.push(reference.clone());
        }
        for need in needs(&bundle.revisions) {
            if bundle
                .references
                .iter()
                .any(|r| r.collection == need.collection && r.id == need.id)
            {
                continue;
            }
            if !have_store || self.local_record(&need).is_err() {
                return Err(ToolResponse::refused(
                    Code::UnknownReference,
                    &need.location,
                    format!(
                        "{} ({} {}) is not carried by the bundle and not retained in this workspace; export the project again from a workspace that has it, or load it here first",
                        need.what, need.collection, need.id
                    ),
                ));
            }
        }
        Ok(incoming)
    }
}
