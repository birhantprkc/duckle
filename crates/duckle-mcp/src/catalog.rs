//! Component catalog, embedded from a committed catalog.json that is exported
//! from the frontend manifest (`npm --prefix frontend run export-catalog`).
//!
//! The catalog is read loosely as a `serde_json::Value` so the Rust side does
//! not have to track every field the frontend manifest emits: the TS manifest
//! stays the single source of truth and this module just indexes + filters it.

use serde_json::{json, Value};
use std::sync::OnceLock;

const CATALOG_JSON: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/catalog.json"));

fn catalog() -> &'static Value {
    static C: OnceLock<Value> = OnceLock::new();
    C.get_or_init(|| serde_json::from_str(CATALOG_JSON).unwrap_or_else(|_| json!({ "components": [] })))
}

fn components() -> &'static [Value] {
    static EMPTY: Vec<Value> = Vec::new();
    catalog()
        .get("components")
        .and_then(|c| c.as_array())
        .map(|v| v.as_slice())
        .unwrap_or(&EMPTY)
}

/// List components, optionally filtered by `kind` (source/transform/sink/
/// control/quality/custom) and/or a case-insensitive substring `query` over
/// id/label/summary. Returns only the summary fields (not the full schema).
pub fn list(kind: Option<&str>, query: Option<&str>) -> Value {
    let q = query.map(|s| s.to_lowercase());
    let items: Vec<Value> = components()
        .iter()
        .filter(|c| {
            let ok_kind = kind.map_or(true, |want| {
                c.get("kind").and_then(|v| v.as_str()) == Some(want)
            });
            let ok_q = match &q {
                Some(q) => {
                    let hay = |k: &str| {
                        c.get(k)
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_lowercase()
                    };
                    hay("id").contains(q.as_str())
                        || hay("label").contains(q.as_str())
                        || hay("summary").contains(q.as_str())
                }
                None => true,
            };
            ok_kind && ok_q
        })
        .map(|c| {
            json!({
                "id": c.get("id"),
                "label": c.get("label"),
                "kind": c.get("kind"),
                "availability": c.get("availability"),
                "summary": c.get("summary"),
            })
        })
        .collect();
    json!({ "count": items.len(), "components": items })
}

/// Full schema (property fields + ports) for one component id, or None.
pub fn schema(id: &str) -> Option<Value> {
    components()
        .iter()
        .find(|c| c.get("id").and_then(|v| v.as_str()) == Some(id))
        .cloned()
}

/// The whole embedded catalog (for the duckle://catalog resource).
pub fn full() -> &'static Value {
    catalog()
}

#[cfg(test)]
mod tests {

    /// xf.ai.chunk slices characters, and says so.
    ///
    /// `run_ai_chunk` passes chunkSize straight into `chunk_text`, which windows
    /// `text.chars()`: no tokenizer is involved anywhere in the workspace. The
    /// form said "(tokens)", so someone asking for 512 got about a quarter of
    /// the text they meant, and this catalog taught the same wrong unit to
    /// anything authoring a pipeline through the MCP server.
    ///
    /// The defaults belong here too. A form `defaultValue` is only a display
    /// fallback and is never written into the node, so an untouched node runs
    /// the engine's own fallback and the form has to show THAT number.
    #[test]
    fn ai_chunk_sizes_are_labelled_in_the_unit_the_engine_slices() {
        let schema = super::schema("xf.ai.chunk").expect("xf.ai.chunk missing from the catalog");
        let sections = schema["manifest"]["sections"]
            .as_array()
            .expect("xf.ai.chunk declares no sections")
            .clone();
        let fields: Vec<&serde_json::Value> = sections
            .iter()
            .flat_map(|s| s["fields"].as_array().expect("a section declares no fields"))
            .collect();
        // The engine's own fallbacks, plan/mod.rs: chunkSize 1000, overlap 100.
        for (key, engine_fallback) in [("chunkSize", 1000u64), ("chunkOverlap", 100u64)] {
            let f = fields
                .iter()
                .find(|f| f["key"].as_str() == Some(key))
                .unwrap_or_else(|| panic!("xf.ai.chunk declares no {key}"));
            let label = f["label"].as_str().unwrap_or_default();
            assert!(
                !label.contains("token"),
                "{key} is labelled {label:?}, but the splitter windows chars"
            );
            assert!(
                label.contains("character"),
                "{key} must name its unit, got {label:?}"
            );
            assert_eq!(
                f["defaultValue"].as_u64(),
                Some(engine_fallback),
                "{key}'s displayed default is not what an untouched node runs"
            );
        }
    }
    /// Components whose engine builder requires a second input, and so must
    /// declare a `lookup` port for the canvas to let anyone wire one up.
    ///
    /// The canvas gates the lookup connection on `manifest.ports.inputs`
    /// (Canvas.tsx), and the engine reads the second layer via
    /// `NodeInputs::first_lookup()`, which resolves the handle named `lookup`
    /// (plan/graph.rs). If the two disagree the node is unusable: the engine
    /// fails with "needs a ... on the second input" and the UI offers no port
    /// to satisfy it. That is exactly what shipped for Clip and Erase in
    /// v0.5.9 (#217, #218), so this pins the contract.
    const NEEDS_LOOKUP_INPUT: &[&str] = &[
        "xf.join",
        "xf.join.cross",
        "xf.join.spatial",
        "xf.lookup",
        "xf.semi",
        "xf.anti",
        "xf.cdc.diff",
        "xf.cdc.scd1",
        "xf.cdc.scd2",
        "xf.cdc.scd3",
        "xf.cdc.upsert",
        "qa.refintegrity",
        "qa.link",
        "qa.reconcile",
        "qa.block",
        "xf.geo.clip",
        "xf.geo.erase",
    ];

    #[test]
    fn two_input_components_declare_a_lookup_port() {
        for id in NEEDS_LOOKUP_INPUT {
            let schema = super::schema(id).unwrap_or_else(|| panic!("{id} missing from catalog"));
            let inputs = schema
                .get("ports")
                .and_then(|p| p.get("inputs"))
                .and_then(|v| v.as_array())
                .unwrap_or_else(|| panic!("{id} declares no input ports"));
            let has_lookup = inputs.iter().any(|p| {
                p.get("type").and_then(|v| v.as_str()) == Some("lookup")
                    || p.get("id")
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| s.starts_with("lookup"))
            });
            assert!(
                has_lookup,
                "{id} needs a second input but the catalog declares only {:?}. \
                 The canvas will not offer the connection and the node cannot run.",
                inputs
                    .iter()
                    .filter_map(|p| p.get("id").and_then(|v| v.as_str()))
                    .collect::<Vec<_>>()
            );
        }
    }
}
