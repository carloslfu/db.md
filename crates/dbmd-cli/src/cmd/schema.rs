// SPDX-License-Identifier: Apache-2.0

//! `dbmd schema [<type>]` — print the store's declared type contracts.
//!
//! Thin wrapper: open the store (its `DB.md ## Schemas` section is already
//! parsed at open into `store.config.schemas`) and render the declared
//! schemas — each field with its modifiers, plus the `unique:` keys,
//! `summary_template`, and `shard` directives — as text or structured JSON
//! (`--json`). This is the introspection twin of schema *enforcement*
//! (`dbmd validate`): an app or agent reads the contract here instead of
//! re-parsing `DB.md`. All parsing lives in `dbmd_core::parser::parse_db_md`;
//! this body only selects and formats.
//!
//! A type with no `### <type>` block is unconstrained: selecting it prints
//! nothing and exits 0 ("no schema" is a valid answer, not an error), the
//! same pipe-safe empty-output convention `dbmd sections` uses.

use dbmd_core::parser::{FieldSpec, Schema, Shape};

use crate::cli::SchemaArgs;
use crate::cmd::write::open_store;
use crate::context::Context;
use crate::error::CliResult;
use crate::sanitize::sanitize_single_line;

/// Run `dbmd schema`.
pub fn run(ctx: &Context, args: &SchemaArgs) -> CliResult {
    let store = open_store(&args.dir)?;
    let selected: Vec<(&String, &Schema)> = store
        .config
        .schemas
        .iter()
        .filter(|(name, _)| match &args.r#type {
            Some(t) => *name == t,
            None => true,
        })
        .collect();

    if ctx.json {
        print!("{}", schemas_json(&selected));
    } else {
        print!("{}", schemas_text(&selected));
    }
    Ok(())
}

/// Human form: each type rendered back in the `DB.md ## Schemas` bullet
/// syntax it was declared in (`### <type>` + `- <field> (<modifiers>)` +
/// directive bullets), so the output is itself valid schema source. Types
/// are separated by one blank line; no declared schemas prints nothing
/// (pipe-safe).
fn schemas_text(schemas: &[(&String, &Schema)]) -> String {
    let mut out = String::new();
    for (i, (name, schema)) in schemas.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!("### {}\n", sanitize_single_line(name)));
        for field in &schema.fields {
            out.push_str(&sanitize_single_line(&field_bullet(field)));
            out.push('\n');
        }
        for key in &schema.unique_keys {
            out.push_str(&sanitize_single_line(&format!(
                "- unique: {}",
                key.join(", ")
            )));
            out.push('\n');
        }
        if let Some(template) = &schema.summary_template {
            out.push_str(&sanitize_single_line(&format!(
                "- summary_template: {template}"
            )));
            out.push('\n');
        }
        if let Some(shard) = schema.shard {
            out.push_str(&format!("- shard: {}\n", shard_word(shard)));
        }
    }
    out
}

/// Machine form: `{"types": {<name>: {fields, unique, summary_template,
/// shard}}}` with a uniform per-field shape (absent modifiers render as
/// `null` / `[]`, the `emit` convention for loaders). Pretty-printed with a
/// trailing newline, the one-shot-verb convention.
fn schemas_json(schemas: &[(&String, &Schema)]) -> String {
    let mut types = serde_json::Map::new();
    for (name, schema) in schemas {
        types.insert(name.to_string(), schema_json(schema));
    }
    let obj = serde_json::json!({ "types": types });
    let mut s = serde_json::to_string_pretty(&obj).unwrap_or_else(|_| "{}".to_string());
    s.push('\n');
    s
}

/// One declared schema as JSON.
fn schema_json(schema: &Schema) -> serde_json::Value {
    serde_json::json!({
        "fields": schema.fields.iter().map(field_json).collect::<Vec<_>>(),
        "unique": schema.unique_keys,
        "summary_template": schema.summary_template,
        "shard": schema.shard.map(shard_word),
    })
}

/// One field declaration as JSON. `default` rides as its parsed YAML value
/// re-encoded to JSON (the same projection `emit` uses for frontmatter).
fn field_json(field: &FieldSpec) -> serde_json::Value {
    serde_json::json!({
        "name": field.name,
        "required": field.required,
        "shape": field.shape.map(shape_word),
        "link_prefix": field
            .link_prefix
            .as_ref()
            .map(|p| p.to_string_lossy().replace('\\', "/")),
        "default": field
            .default
            .as_ref()
            .map(|v| serde_json::to_value(v).unwrap_or(serde_json::Value::Null)),
        "enum": field.enum_values,
        "unknown_modifiers": field.unknown_modifiers,
    })
}

/// Re-render one field bullet in the canonical `- <name> (<modifiers>)`
/// source syntax, modifiers in the vocabulary's documentation order.
fn field_bullet(field: &FieldSpec) -> String {
    let mut mods: Vec<String> = Vec::new();
    if field.required {
        mods.push("required".to_string());
    }
    if let Some(shape) = field.shape {
        mods.push(shape_word(shape).to_string());
    }
    if let Some(prefix) = &field.link_prefix {
        mods.push(format!(
            "link to {}/",
            prefix.to_string_lossy().replace('\\', "/")
        ));
    }
    if let Some(default) = &field.default {
        mods.push(format!("default {}", scalar_text(default)));
    }
    if let Some(values) = &field.enum_values {
        mods.push(format!("enum: {}", values.join(", ")));
    }
    mods.extend(field.unknown_modifiers.iter().cloned());
    if mods.is_empty() {
        format!("- {}", field.name)
    } else {
        format!("- {} ({})", field.name, mods.join(", "))
    }
}

/// A YAML scalar rendered as bare text for the bullet form (strings unquoted,
/// everything else in its YAML spelling).
fn scalar_text(value: &serde_norway::Value) -> String {
    match value {
        serde_norway::Value::String(s) => s.clone(),
        other => serde_norway::to_string(other)
            .map(|s| s.trim_end().to_string())
            .unwrap_or_default(),
    }
}

/// The shape modifier's source word.
fn shape_word(shape: Shape) -> &'static str {
    match shape {
        Shape::String => "string",
        Shape::Int => "int",
        Shape::Bool => "bool",
        Shape::Date => "date",
        Shape::Email => "email",
        Shape::Currency => "currency",
        Shape::Url => "url",
    }
}

/// The `shard:` directive word for the parsed flag.
fn shard_word(by_date: bool) -> &'static str {
    if by_date {
        "by-date"
    } else {
        "flat"
    }
}
