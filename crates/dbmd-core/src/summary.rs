//! `summary` — the deterministic default-`summary` composer.
//!
//! Used by `dbmd fm init` and `dbmd write` when the agent doesn't supply a
//! `summary`. [`compose_default`] renders the type's `summary_template` (from
//! the store's `DB.md ## Schemas`) when one is declared, and otherwise falls
//! back to the body's first non-heading paragraph. No type carries a built-in
//! template — the template, like the schema, is the store's to declare.
//!
//! Contract: **deterministic** (same `(type, frontmatter, body)` → same
//! string), **single-line** (newlines collapsed to spaces), and **capped at 200
//! chars** (the SPEC readability bound). The tool generates a deterministic
//! floor; the agent provides the ceiling via `dbmd fm set <file> summary='…'`.

use serde_norway::Value;

use crate::parser::Frontmatter;
use crate::store::Store;

/// The SPEC's `summary` length bound, in characters.
pub const MAX_SUMMARY_LEN: usize = 200;

/// Compose a deterministic default `summary` for a file from its `type`,
/// frontmatter, and body. If the store's `## Schemas` declares a
/// `summary_template` for the type, it is rendered with `{field}` interpolation;
/// otherwise the default is the body's first non-heading paragraph. The result
/// is always single-line and ≤ [`MAX_SUMMARY_LEN`] chars.
///
/// The tool generates a deterministic floor; the agent provides the ceiling via
/// `dbmd fm set <file> summary='…'`.
pub fn compose_default(
    store: &Store,
    type_: &str,
    frontmatter: &Frontmatter,
    body: &str,
) -> crate::Result<String> {
    let composed = match store
        .config
        .schemas
        .get(type_)
        .and_then(|s| s.summary_template.as_deref())
    {
        Some(template) => render_template(template, frontmatter),
        None => compose_from_body(body),
    };
    Ok(normalize(&composed))
}

/// Render a `summary_template` — substitute each `{field}` with the file's
/// frontmatter value for `field`. A scalar (incl. a wiki-link, reduced to its
/// display-or-leaf form) renders inline; a list renders comma-joined; an
/// absent/empty field renders empty. An unmatched `{` is emitted verbatim
/// (templates are simple field-interpolation floors, not a templating language).
fn render_template(template: &str, fm: &Frontmatter) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let close = after.find('}');
        let next_open = after.find('{');
        match close {
            // A clean `{field}` — no nested `{` before the closing `}`.
            Some(c) if next_open.is_none_or(|n| n > c) => {
                let key = after[..c].trim();
                if let Some(scalar) = field_text(fm, key) {
                    out.push_str(&scalar);
                } else {
                    let list = list_field_texts(fm, key);
                    if !list.is_empty() {
                        out.push_str(&list.join(", "));
                    }
                }
                rest = &after[c + 1..];
            }
            // A stray `{` (no `}`, or another `{` first) — emit it verbatim.
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The body fallback: the file's first non-heading paragraph, truncated to
/// [`MAX_SUMMARY_LEN`] chars (the truncation is applied by [`normalize`]).
pub fn compose_from_body(body: &str) -> String {
    first_paragraph(body).unwrap_or_default()
}

/// Normalize any candidate summary to the contract: collapse runs of
/// whitespace (including newlines) to single spaces, trim, and truncate to
/// [`MAX_SUMMARY_LEN`] **chars** (never splitting a UTF-8 codepoint). Every
/// composer runs its output through this.
pub fn normalize(candidate: &str) -> String {
    // `split_whitespace` collapses any run of ASCII/Unicode whitespace
    // (spaces, tabs, newlines) and trims leading/trailing — giving the
    // single-line, trimmed form in one pass.
    let collapsed = candidate.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&collapsed, MAX_SUMMARY_LEN)
}

// ── Internal helpers ────────────────────────────────────────────────────────

/// Truncate to at most `max` Unicode scalar values, on a char boundary.
fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((byte_idx, _)) => s[..byte_idx].to_string(),
        None => s.to_string(),
    }
}

/// Read a frontmatter field's raw YAML value, checking the universal typed
/// fields first and then [`Frontmatter::extra`] — mirroring the documented
/// contract of `Frontmatter::get` but reading the struct directly so this
/// module never depends on another module's body.
fn field_value(fm: &Frontmatter, key: &str) -> Option<Value> {
    match key {
        "type" => fm.type_.clone().map(Value::String),
        "id" => fm.id.clone().map(Value::String),
        "summary" => fm.summary.clone().map(Value::String),
        "status" => fm.status.clone().map(Value::String),
        // Typed universal fields a `summary_template` may legitimately
        // interpolate. `created`/`updated` render as their canonical RFC3339
        // string; `tags` as a sequence (which `list_field_texts` comma-joins).
        // Without these arms, `{created}` / `{updated}` / `{tags}` would
        // silently render empty even when the value is present.
        "created" => fm.created.map(|t| Value::String(t.to_rfc3339())),
        "updated" => fm.updated.map(|t| Value::String(t.to_rfc3339())),
        "tags" => {
            if fm.tags.is_empty() {
                None
            } else {
                Some(Value::Sequence(
                    fm.tags.iter().cloned().map(Value::String).collect(),
                ))
            }
        }
        _ => fm.extra.get(key).cloned(),
    }
}

/// Read a single frontmatter field as a rendered plain-text scalar, or `None`
/// when the field is absent, null, or renders empty. Wiki-link-valued fields
/// are reduced to their display-or-leaf human form (never the raw `[[...]]`).
fn field_text(fm: &Frontmatter, key: &str) -> Option<String> {
    let v = field_value(fm, key)?;
    let rendered = render_scalar(&v)?;
    let trimmed = rendered.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Read a list-valued frontmatter field as rendered plain-text items. A scalar
/// (non-sequence) value is treated as a single-item list. Wiki-link items are
/// reduced to their display-or-leaf form. Empty / null items are dropped.
fn list_field_texts(fm: &Frontmatter, key: &str) -> Vec<String> {
    let Some(v) = field_value(fm, key) else {
        return Vec::new();
    };
    match v {
        Value::Sequence(items) => items
            .iter()
            .filter_map(|item| {
                let r = render_scalar(item)?;
                let t = r.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_string())
                }
            })
            .collect(),
        other => render_scalar(&other)
            .map(|r| r.trim().to_string())
            .filter(|t| !t.is_empty())
            .into_iter()
            .collect(),
    }
}

/// Render a single YAML scalar to plain display text. Strings (including YAML
/// date scalars, which deserialize as strings) are returned as-is but with any
/// wiki-link reduced to display-or-leaf; numbers and bools stringify
/// canonically; null / mapping / nested-sequence yield `None`.
fn render_scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(reduce_wiki_link(s)),
        Value::Sequence(_) => render_unquoted_wiki_link(v),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => {
            // Render integers without a trailing `.0`; keep the natural form
            // otherwise. `Number`'s Display already does this.
            Some(n.to_string())
        }
        Value::Null | Value::Mapping(_) | Value::Tagged(_) => None,
    }
}

/// YAML parses an unquoted wiki-link scalar (`company: [[records/x]]`) as a
/// nested sequence, not a string. Recognize that shape so summary templates
/// render it exactly like the quoted scalar form.
fn render_unquoted_wiki_link(v: &Value) -> Option<String> {
    let Value::Sequence(outer) = v else {
        return None;
    };
    if outer.len() != 1 {
        return None;
    }
    let Value::Sequence(inner) = &outer[0] else {
        return None;
    };
    let [Value::String(target)] = inner.as_slice() else {
        return None;
    };
    Some(reduce_wiki_link(&format!("[[{target}]]")))
}

/// If `s` is a wiki-link (`[[target]]` or `[[target|display]]`), reduce it to
/// the human form: the `display` override when present, else the last path
/// segment of the target (with any `.md` suffix dropped). Non-link strings are
/// returned unchanged.
fn reduce_wiki_link(s: &str) -> String {
    let trimmed = s.trim();
    let inner = trimmed
        .strip_prefix("[[")
        .and_then(|rest| rest.strip_suffix("]]"));
    let Some(inner) = inner else {
        return s.to_string();
    };
    // `target|display` → prefer display.
    let (target, display) = match inner.split_once('|') {
        Some((t, d)) => (t, Some(d)),
        None => (inner, None),
    };
    if let Some(d) = display {
        let d = d.trim();
        if !d.is_empty() {
            return d.to_string();
        }
    }
    let leaf = target.trim().rsplit('/').next().unwrap_or(target).trim();
    leaf.strip_suffix(".md").unwrap_or(leaf).to_string()
}

/// The first non-heading, non-blank paragraph of the body: consecutive
/// non-heading text lines joined by a space, starting at the first such line.
/// Heading lines (`#…`) are skipped. `None` when the body has no prose.
fn first_paragraph(body: &str) -> Option<String> {
    let mut collected: Vec<&str> = Vec::new();
    for line in body.lines() {
        let t = line.trim();
        if t.is_empty() {
            if collected.is_empty() {
                // Still searching for the start of the first paragraph.
                continue;
            }
            // Blank line ends the first paragraph.
            break;
        }
        if t.starts_with('#') {
            if collected.is_empty() {
                // A heading before any prose — skip it.
                continue;
            }
            // A heading terminates the running paragraph.
            break;
        }
        collected.push(t);
    }
    if collected.is_empty() {
        None
    } else {
        Some(collected.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{Config, Schema};
    use std::fs;
    use tempfile::TempDir;

    // ── Fixtures ─────────────────────────────────────────────────────────────

    /// A temp store with a `DB.md` marker and the given parsed config, built
    /// directly (not via `Store::open`) so these tests exercise the `summary`
    /// code under test, not store-open plumbing.
    fn store_with(config: Config) -> (TempDir, Store) {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path().to_path_buf();
        fs::write(root.join("DB.md"), "---\ntype: db-md\n---\n").expect("write DB.md");
        let store = Store { root, config };
        (tmp, store)
    }

    /// A store whose `## Schemas` declares a `summary_template` for `type_`.
    fn store_with_template(type_: &str, template: &str) -> (TempDir, Store) {
        let mut config = Config::default();
        config.schemas.insert(
            type_.to_string(),
            Schema {
                summary_template: Some(template.to_string()),
                ..Schema::default()
            },
        );
        store_with(config)
    }

    /// Build a [`Frontmatter`] from a YAML map literal so tests state intent in
    /// YAML, not by hand-poking `extra`. This goes through `serde_norway` exactly
    /// like a real file's frontmatter would.
    fn fm(yaml: &str) -> Frontmatter {
        let value: Value = serde_norway::from_str(yaml).expect("test yaml parses");
        let mapping = value.as_mapping().expect("test yaml is a mapping").clone();
        let mut f = Frontmatter::default();
        for (k, v) in mapping {
            let key = k.as_str().expect("string key").to_string();
            match key.as_str() {
                "type" => f.type_ = v.as_str().map(str::to_string),
                "summary" => f.summary = v.as_str().map(str::to_string),
                "id" => f.id = v.as_str().map(str::to_string),
                "status" => f.status = v.as_str().map(str::to_string),
                // Route the typed universal fields to their struct slots (NOT
                // `extra`) so tests exercise the real `field_value` arms for
                // `{tags}` / `{created}` / `{updated}` instead of masking them.
                "tags" => {
                    if let Value::Sequence(items) = &v {
                        f.tags = items
                            .iter()
                            .filter_map(|i| i.as_str().map(str::to_string))
                            .collect();
                    }
                }
                "created" => {
                    f.created = v
                        .as_str()
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                }
                "updated" => {
                    f.updated = v
                        .as_str()
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                }
                _ => {
                    f.extra.insert(key, v);
                }
            }
        }
        f
    }

    // ── normalize ────────────────────────────────────────────────────────────

    #[test]
    fn normalize_collapses_newlines_and_runs_to_single_spaces() {
        let got = normalize("first line\nsecond\t\tline   third");
        assert_eq!(got, "first line second line third");
    }

    #[test]
    fn normalize_trims_surrounding_whitespace() {
        assert_eq!(normalize("   padded value \n"), "padded value");
    }

    #[test]
    fn normalize_caps_at_200_chars_on_char_boundary() {
        // 250 multi-byte chars; the cap is by char, not byte.
        let input = "é".repeat(250);
        let got = normalize(&input);
        assert_eq!(got.chars().count(), MAX_SUMMARY_LEN);
        // Truncation must not corrupt UTF-8 (would panic on slice otherwise).
        assert_eq!(got, "é".repeat(MAX_SUMMARY_LEN));
    }

    #[test]
    fn normalize_leaves_short_strings_untouched() {
        assert_eq!(normalize("short"), "short");
    }

    // ── summary_template rendering ───────────────────────────────────────────

    #[test]
    fn template_interpolates_scalar_fields() {
        let (_t, store) =
            store_with_template("contact", "{role} at {company} (last_touch: {last_touch})");
        let f = fm("type: contact\n\
             role: Director of Operations\n\
             company: \"[[records/companies/northstar]]\"\n\
             last_touch: 2026-05-22\n");
        // A wiki-link value reduces to its leaf; the template is the store's, not
        // a built-in — that is the whole point.
        assert_eq!(
            compose_default(&store, "contact", &f, "ignored body").unwrap(),
            "Director of Operations at northstar (last_touch: 2026-05-22)"
        );
    }

    #[test]
    fn template_interpolates_unquoted_scalar_wiki_link_fields() {
        let (_t, store) = store_with_template("contact", "{role} at {company}");
        let f = fm("type: contact\n\
             role: Director\n\
             company: [[records/companies/northstar]]\n");
        assert_eq!(
            compose_default(&store, "contact", &f, "").unwrap(),
            "Director at northstar"
        );
    }

    #[test]
    fn template_drops_absent_fields_to_empty() {
        let (_t, store) = store_with_template("contact", "{role} at {company}");
        let f = fm("type: contact\nrole: Advisor\n");
        // `{company}` absent → empty; `normalize` trims the trailing run.
        assert_eq!(
            compose_default(&store, "contact", &f, "").unwrap(),
            "Advisor at"
        );
    }

    #[test]
    fn template_joins_list_fields_comma_separated() {
        let (_t, store) = store_with_template("meeting", "{date}: {attendees}");
        let f = fm("type: meeting\n\
             date: 2026-05-10\n\
             attendees:\n\
             \x20 - \"[[records/contacts/alice]]\"\n\
             \x20 - \"[[records/contacts/bob]]\"\n");
        assert_eq!(
            compose_default(&store, "meeting", &f, "").unwrap(),
            "2026-05-10: alice, bob"
        );
    }

    #[test]
    fn template_interpolates_typed_tags_created_updated() {
        // Regression: `field_value` skipped the typed `tags` / `created` /
        // `updated` fields, so these `{…}` placeholders silently rendered empty
        // even when the values were present.
        let (_t, store) = store_with_template("note", "{tags} | {created}");
        let f = fm("type: note\ntags: [urgent, q3]\ncreated: \"2026-05-01T00:00:00Z\"\n");
        assert_eq!(
            compose_default(&store, "note", &f, "").unwrap(),
            // {tags} comma-joins; {created} renders canonical RFC3339 (offset form).
            "urgent, q3 | 2026-05-01T00:00:00+00:00"
        );
    }

    #[test]
    fn template_joins_unquoted_block_wiki_link_list_fields() {
        let (_t, store) = store_with_template("meeting", "{attendees}");
        let f = fm("type: meeting\n\
             attendees:\n\
             \x20 - [[records/contacts/alice]]\n\
             \x20 - [[records/contacts/bob]]\n");
        assert_eq!(
            compose_default(&store, "meeting", &f, "").unwrap(),
            "alice, bob"
        );
    }

    #[test]
    fn template_emits_stray_brace_verbatim() {
        let (_t, store) = store_with_template("note", "literal { brace {title}");
        let f = fm("type: note\ntitle: Hello\n");
        assert_eq!(
            compose_default(&store, "note", &f, "").unwrap(),
            "literal { brace Hello"
        );
    }

    #[test]
    fn template_is_deterministic_across_calls() {
        let (_t, store) = store_with_template("contact", "{role} ({last_touch})");
        let f = fm("type: contact\nrole: Ops Lead\nlast_touch: 2026-05-22\n");
        let a = compose_default(&store, "contact", &f, "body").unwrap();
        let b = compose_default(&store, "contact", &f, "body").unwrap();
        assert_eq!(a, b);
        assert_eq!(a, "Ops Lead (2026-05-22)");
    }

    #[test]
    fn no_schema_for_type_falls_back_to_body() {
        // Only `contact` has a template; `note` falls back to the body paragraph,
        // proving no type carries a built-in template.
        let (_t, store) = store_with_template("contact", "{role}");
        let f = fm("type: note\n");
        assert_eq!(
            compose_default(&store, "note", &f, "Body sentence here.").unwrap(),
            "Body sentence here."
        );
    }

    // ── unknown / custom + body extraction ─────────────────────────────────────

    #[test]
    fn unknown_type_uses_first_non_heading_paragraph() {
        let (_t, store) = store_with(Config::default());
        let f = fm("type: proposal\n");
        let body = "# Title\n\nThis proposal covers the Q3 roadmap.\n\nSecond paragraph.\n";
        let got = compose_default(&store, "proposal", &f, body).unwrap();
        assert_eq!(got, "This proposal covers the Q3 roadmap.");
    }

    #[test]
    fn first_paragraph_joins_wrapped_lines_until_blank() {
        let body = "Line one\nline two\n\nlater paragraph";
        assert_eq!(first_paragraph(body).as_deref(), Some("Line one line two"));
    }

    #[test]
    fn first_paragraph_none_for_heading_only_body() {
        assert_eq!(first_paragraph("# Just a heading\n## And another\n"), None);
    }

    #[test]
    fn unknown_type_long_paragraph_is_capped_at_200() {
        let (_t, store) = store_with(Config::default());
        let f = fm("type: note\n");
        let long = "word ".repeat(100); // 500 chars
        let got = compose_default(&store, "note", &f, &long).unwrap();
        assert!(got.chars().count() <= MAX_SUMMARY_LEN);
        assert!(got.chars().count() >= MAX_SUMMARY_LEN - 5); // close to the cap
    }

    // ── wiki-link reduction ────────────────────────────────────────────────────

    #[test]
    fn reduce_wiki_link_takes_leaf_segment() {
        assert_eq!(
            reduce_wiki_link("[[records/companies/northstar]]"),
            "northstar"
        );
    }

    #[test]
    fn reduce_wiki_link_prefers_display() {
        assert_eq!(
            reduce_wiki_link("[[records/companies/x|Northstar Inc]]"),
            "Northstar Inc"
        );
    }

    #[test]
    fn reduce_wiki_link_strips_md_extension() {
        assert_eq!(reduce_wiki_link("[[records/companies/x.md]]"), "x");
    }

    #[test]
    fn reduce_wiki_link_passes_through_plain_text() {
        assert_eq!(reduce_wiki_link("just a vendor name"), "just a vendor name");
    }
}
