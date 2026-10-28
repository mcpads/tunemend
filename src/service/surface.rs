//! A small production surface; specialist operations are discovered on demand.
use super::*;

const DIRECT: &[&str] = &[
    "get_capabilities",
    "create_project",
    "inspect_project",
    "edit_project",
    "check_project",
    "render_preview",
    "present_review",
    "export_project",
];

pub fn operation_group(name: &str) -> Option<&'static str> {
    Some(match name {
        "load_midi" | "inspect_midi" | "export_midi_source" | "compare_midi" | "analyze_midi"
        | "import_midi" => "midi",
        "update_brief" | "set_head" | "compare_revisions" | "import_project" => "project",
        "render_batch" | "measure_audio" | "compare_audio" => "audio",
        "read_feedback" | "record_feedback" => "feedback",
        "prepare_sample" | "inspect_sample" | "export_sample" => "sample",
        "attach_observation" | "inspect_observation" | "export_observation" => "observation",
        "prepare_target" | "inspect_target" | "export_target" | "convert_for_target" => "target",
        _ if DIRECT.contains(&name) => return None,
        "load_fm_bank"
        | "inspect_fm_bank"
        | "copy_fm_patch"
        | "export_fm_bank"
        | "render_fm_patch"
        | "load_vab_bank"
        | "inspect_vab_bank"
        | "replace_vab_sample"
        | "export_vab_bank"
        | "render_vab_tone"
        | "load_psx_sequence"
        | "inspect_psx_sequence"
        | "export_psx_sequence"
        | "render_psx_sequence"
        | "load_instrument_source"
        | "inspect_instrument_source"
        | "export_instrument_source"
        | "inspect_vgm"
        | "render_psg"
        | "transform_vgm"
        | "inspect_adx"
        | "render_adx"
        | "rank_adx_loops"
        | "encode_adx"
        | "inspect_nds_sound"
        | "export_nds_sample"
        | "replace_nds_wave"
        | "export_instrument_bank"
        | "load_instrument_bank"
        | "inspect_instrument_bank"
        | "copy_instrument"
        | "render_instrument" => "sound",
        _ => return None,
    })
}

const GROUPS: &[(&str, &str)] = &[
    (
        "midi",
        "Retain, inspect, compare and import MIDI, or export exact originals.",
    ),
    (
        "project",
        "Resume a bundle, choose a revision, compare revisions or update the brief.",
    ),
    (
        "audio",
        "Render stems and regions, measure audio or compare recordings.",
    ),
    (
        "feedback",
        "Read listening marks or record the person's feedback.",
    ),
    (
        "sample",
        "Prepare, inspect and export sample conversions with explicit losses.",
    ),
    (
        "sound",
        "Inspect, edit, audition and export native banks, sequences and streams.",
    ),
    (
        "target",
        "Prepare and deliver explicitly mapped consumer assets.",
    ),
    (
        "observation",
        "Retain, inspect and export external capture evidence.",
    ),
];

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RoutedInput {
    /// describe lists operations; otherwise use an operation from that list.
    operation: String,
    /// With describe, select one operation to get its exact argument schema.
    #[serde(default)]
    select: Option<String>,
    /// Arguments of the selected operation, including its job_id when required.
    #[serde(default)]
    arguments: Option<serde_json::Map<String, Value>>,
}

pub fn surface_definitions() -> Vec<ToolDefinition> {
    let mut result: Vec<_> = tool_definitions()
        .into_iter()
        .filter(|d| DIRECT.contains(&d.name))
        .collect();
    result.extend(GROUPS.iter().map(|&(name, description)| ToolDefinition {
        name,
        description,
        read_only: false,
        input_schema: schemars::schema_for!(RoutedInput),
    }));
    result
}

impl Service {
    pub(super) fn route(&mut self, tool: &str, arguments: &Value) -> Option<ToolResponse> {
        if !GROUPS.iter().any(|(name, _)| *name == tool) {
            return None;
        }
        Some(self.route_inner(tool, arguments).unwrap_or_else(|e| e))
    }

    fn route_inner(
        &mut self,
        group: &str,
        arguments: &Value,
    ) -> Result<ToolResponse, ToolResponse> {
        let input: RoutedInput = serde_json::from_value(arguments.clone())
            .map_err(|e| ToolResponse::refused(Code::SchemaMismatch, "", e.to_string()))?;
        let definitions: Vec<_> = tool_definitions()
            .into_iter()
            .filter(|d| operation_group(d.name) == Some(group))
            .collect();
        if input.operation == "describe" {
            if input.arguments.is_some() {
                return Err(ToolResponse::refused(
                    Code::SchemaMismatch,
                    "/arguments",
                    "describe takes select, not execution arguments",
                ));
            }
            if let Some(select) = input.select {
                let d = definitions
                    .iter()
                    .find(|d| d.name == select)
                    .ok_or_else(|| {
                        ToolResponse::refused(
                            Code::UnknownReference,
                            "/select",
                            "operation is not in this group; use describe without select",
                        )
                    })?;
                return Ok(ToolResponse::ok(
                    "operation schema",
                    json!({
                        "tool":group, "operation":d.name, "description":d.description,
                        "read_only":d.read_only, "arguments_schema":d.input_schema
                    }),
                ));
            }
            return Ok(ToolResponse::ok(
                "available operations",
                json!({
                    "tool":group, "operations":definitions.iter().map(|d| json!({
                        "operation":d.name,"description":d.description,"read_only":d.read_only
                    })).collect::<Vec<_>>(),
                    "next":"Call this tool with operation=describe and select=<operation> for its schema; execute with operation=<operation> and arguments={...}."
                }),
            ));
        }
        if input.select.is_some() {
            return Err(ToolResponse::refused(
                Code::SchemaMismatch,
                "/select",
                "select is only used with describe",
            ));
        }
        if !definitions.iter().any(|d| d.name == input.operation) {
            return Err(ToolResponse::refused(
                Code::UnknownReference,
                "/operation",
                match operation_group(&input.operation) {
                    Some(owner) => format!(
                        "use {owner} with operation={} and the same arguments",
                        input.operation
                    ),
                    None => "unknown operation; use describe for this group's operations".into(),
                },
            ));
        }
        // Delegate before hashing: exact retries retain their original job identity.
        let mut response = self.call(
            &input.operation,
            Value::Object(input.arguments.unwrap_or_default()),
        );
        for d in &mut response.diagnostics {
            if d.code == Code::SchemaMismatch {
                d.location = format!("/arguments{}", d.location);
            }
        }
        Ok(response)
    }
}
