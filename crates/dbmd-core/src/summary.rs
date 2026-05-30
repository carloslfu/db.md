//! `summary` — the deterministic default-`summary` composer.
//!
//! Used by `dbmd fm init` and `dbmd write` when the agent doesn't supply a
//! `summary`. [`compose_default`] dispatches on `type` to the per-type
//! composers, matching SPEC.md's "Deterministic defaults per type" table.
//!
//! Contract every composer upholds: **deterministic** (same
//! `(type, frontmatter, body)` → same string), **single-line** (newlines
//! collapsed to spaces), and **capped at 200 chars** (the SPEC readability
//! bound). The tool generates a deterministic floor; the agent provides the
//! ceiling via `dbmd fm set <file> summary='...'`.

use serde_yml::Value;

use crate::parser::Frontmatter;
use crate::store::Store;

/// The SPEC's `summary` length bound, in characters.
pub const MAX_SUMMARY_LEN: usize = 200;

/// Compose a deterministic default `summary` for a file from its `type`,
/// frontmatter, and body. Dispatches to the per-type composer; unknown/custom
/// types fall back to the first non-heading paragraph of the body. The result
/// is always single-line and ≤ [`MAX_SUMMARY_LEN`] chars.
///
/// `store` is passed because some composers resolve a wiki-link to read a
/// related file's field (e.g. `contact` resolves `company` to the company's
/// `name`).
pub fn compose_default(
    store: &Store,
    type_: &str,
    frontmatter: &Frontmatter,
    body: &str,
) -> crate::Result<String> {
    let composed = match type_ {
        "contact" => compose_contact(store, frontmatter)?,
        "company" => compose_company(frontmatter),
        "expense" => compose_expense(frontmatter),
        "meeting" => compose_meeting(frontmatter),
        "decision" => compose_decision(frontmatter, body),
        "invoice" => compose_invoice(frontmatter),
        "email" => compose_email(frontmatter),
        "transcript" => compose_transcript(frontmatter),
        "pdf-source" => compose_pdf_source(frontmatter),
        "wiki-page" => compose_wiki_page(frontmatter, body),
        // Unknown / custom types fall back to the body.
        _ => compose_from_body(body),
    };
    Ok(normalize(&composed))
}

/// `contact` → `<role> at <company-name> (last_touch: <date>)`. Resolves the
/// `company` wiki-link to read the company's `name`.
///
/// Each segment degrades gracefully when its field is absent: with no `role`
/// the line opens `Contact`; with no resolvable company the ` at <company>`
/// segment is dropped; with no `last_touch` the trailing ` (last_touch: …)`
/// parenthetical is dropped. The company name is the resolved company file's
/// `name` field; if the link can't be resolved (missing/unreadable target, or
/// the target has no `name`) the link's own display-or-leaf text is used.
pub fn compose_contact(store: &Store, fm: &Frontmatter) -> crate::Result<String> {
    let role = field_text(fm, "role");
    let company = resolve_company_name(store, fm);
    let last_touch = field_text(fm, "last_touch");

    let mut out = match role {
        Some(r) => r,
        None => "Contact".to_string(),
    };
    if let Some(c) = company {
        out.push_str(" at ");
        out.push_str(&c);
    }
    if let Some(d) = last_touch {
        out.push_str(" (last_touch: ");
        out.push_str(&d);
        out.push(')');
    }
    Ok(out)
}

/// `company` → `<relationship>; <industry>`.
///
/// When only one of the two fields is present, the lone value is returned
/// without the `; ` separator; when both are absent the result is empty (the
/// caller's `normalize` yields `""`, which validate then flags as
/// `SUMMARY_EMPTY` so the agent supplies a real one).
pub fn compose_company(fm: &Frontmatter) -> String {
    join_present(
        "; ",
        [field_text(fm, "relationship"), field_text(fm, "industry")],
    )
}

/// `expense` → `<date> — <amount> <currency> — <vendor>`.
///
/// `<amount> <currency>` collapse to whichever is present; missing top-level
/// segments (date / amount-currency / vendor) drop out of the ` — `-joined
/// line so a partial record still reads cleanly.
pub fn compose_expense(fm: &Frontmatter) -> String {
    let money = join_present(" ", [field_text(fm, "amount"), field_text(fm, "currency")]);
    let money = if money.is_empty() { None } else { Some(money) };
    join_present(
        " — ",
        [field_text(fm, "date"), money, field_text(fm, "vendor")],
    )
}

/// `meeting` → `<date> — <first 3 attendees> (+N more)` when more than three
/// attendees are present.
///
/// Attendees render as their wiki-link display-or-leaf names, comma-joined;
/// the `(+N more)` suffix appears only when `N > 0`.
pub fn compose_meeting(fm: &Frontmatter) -> String {
    let attendees = list_field_texts(fm, "attendees");
    let shown: Vec<String> = attendees.iter().take(3).cloned().collect();
    let extra = attendees.len().saturating_sub(shown.len());

    let people = if shown.is_empty() {
        None
    } else {
        let mut s = shown.join(", ");
        if extra > 0 {
            s.push_str(&format!(" (+{extra} more)"));
        }
        Some(s)
    };

    join_present(" — ", [field_text(fm, "date"), people])
}

/// `decision` → `<decided_by>: <title-or-first-heading>`.
///
/// The title is the first `#` heading text of the body (any depth), falling
/// back to the first non-heading paragraph when the body has no heading.
pub fn compose_decision(fm: &Frontmatter, body: &str) -> String {
    let title = first_heading(body).or_else(|| first_paragraph(body));
    match (field_text(fm, "decided_by"), title) {
        (Some(who), Some(t)) => format!("{who}: {t}"),
        (Some(who), None) => who,
        (None, Some(t)) => t,
        (None, None) => String::new(),
    }
}

/// `invoice` → `<vendor> — <amount> — <status>`.
pub fn compose_invoice(fm: &Frontmatter) -> String {
    join_present(
        " — ",
        [
            field_text(fm, "vendor"),
            field_text(fm, "amount"),
            field_text(fm, "status"),
        ],
    )
}

/// `email` → `<from> → <to> — <subject>`.
///
/// `to` may be a list; it renders comma-joined. The `<from> → <to>` head and
/// the `<subject>` tail each drop out when empty.
pub fn compose_email(fm: &Frontmatter) -> String {
    let to = {
        let list = list_field_texts(fm, "to");
        if list.is_empty() {
            None
        } else {
            Some(list.join(", "))
        }
    };
    let route = match (field_text(fm, "from"), to) {
        (Some(f), Some(t)) => Some(format!("{f} → {t}")),
        (Some(f), None) => Some(f),
        (None, Some(t)) => Some(format!("→ {t}")),
        (None, None) => None,
    };
    join_present(" — ", [route, field_text(fm, "subject")])
}

/// `transcript` → `<recorded_at> — <attendees>`.
pub fn compose_transcript(fm: &Frontmatter) -> String {
    let attendees = {
        let list = list_field_texts(fm, "attendees");
        if list.is_empty() {
            None
        } else {
            Some(list.join(", "))
        }
    };
    join_present(" — ", [field_text(fm, "recorded_at"), attendees])
}

/// `pdf-source` → `<doc_type> from <received_from>`.
pub fn compose_pdf_source(fm: &Frontmatter) -> String {
    match (field_text(fm, "doc_type"), field_text(fm, "received_from")) {
        (Some(dt), Some(rf)) => format!("{dt} from {rf}"),
        (Some(dt), None) => dt,
        (None, Some(rf)) => format!("from {rf}"),
        (None, None) => String::new(),
    }
}

/// `wiki-page` → the `topic` frontmatter field, else the file's first
/// non-heading paragraph.
pub fn compose_wiki_page(fm: &Frontmatter, body: &str) -> String {
    field_text(fm, "topic").unwrap_or_else(|| compose_from_body(body))
}

/// Unknown / custom types → the file's first non-heading paragraph, truncated
/// to [`MAX_SUMMARY_LEN`] chars (the truncation is applied by [`normalize`]).
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
        // `created` / `updated` are typed timestamps; no composer reads them as
        // a field, so we don't reconstruct a Value for them here.
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
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => {
            // Render integers without a trailing `.0`; keep the natural form
            // otherwise. `Number`'s Display already does this.
            Some(n.to_string())
        }
        Value::Null | Value::Sequence(_) | Value::Mapping(_) | Value::Tagged(_) => None,
    }
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

/// Join the present (`Some`, non-empty) values with `sep`, dropping the absent
/// ones. Returns `""` when none are present.
fn join_present<const N: usize>(sep: &str, parts: [Option<String>; N]) -> String {
    parts
        .into_iter()
        .flatten()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(sep)
}

/// The first `#`-prefixed heading's text (any depth), stripped of leading `#`s
/// and surrounding whitespace; `None` if the body has no heading.
fn first_heading(body: &str) -> Option<String> {
    for line in body.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix('#') {
            // Strip the remaining `#`s of a deeper heading, then whitespace.
            let text = rest.trim_start_matches('#').trim();
            if !text.is_empty() {
                return Some(text.to_string());
            }
        }
    }
    None
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

/// Resolve a `contact`'s `company` wiki-link to the company file's `name`
/// frontmatter field. Falls back to the link's display-or-leaf text when the
/// target can't be read or carries no `name`; `None` when there is no `company`
/// field at all.
///
/// Reads the single target file directly (not a store walk) so the cost is
/// O(1). Any I/O or parse failure degrades to the link's own text rather than
/// erroring — composing a default summary must never fail on a dangling link.
fn resolve_company_name(store: &Store, fm: &Frontmatter) -> Option<String> {
    let raw = match field_value(fm, "company")? {
        Value::String(s) => s,
        // A list-valued company is unusual; take the first link if so.
        Value::Sequence(items) => items.iter().find_map(|i| i.as_str().map(str::to_string))?,
        _ => return None,
    };
    let fallback = {
        let f = reduce_wiki_link(&raw);
        let f = f.trim();
        if f.is_empty() {
            None
        } else {
            Some(f.to_string())
        }
    };

    let Some(target) = wiki_link_target(&raw) else {
        return fallback;
    };
    // `target` is a store-relative path without `.md`; load it under the root.
    let mut abs = store.root.join(&target);
    abs.set_extension("md");
    match read_frontmatter_name(&abs) {
        Some(name) if !name.trim().is_empty() => Some(name.trim().to_string()),
        _ => fallback,
    }
}

/// Extract a wiki-link's bare target path (`[[target]]` / `[[target|x]]` →
/// `target`, `.md` suffix stripped). `None` when `s` is not a wiki-link.
fn wiki_link_target(s: &str) -> Option<String> {
    let inner = s
        .trim()
        .strip_prefix("[[")
        .and_then(|rest| rest.strip_suffix("]]"))?;
    let target = inner
        .split_once('|')
        .map(|(t, _)| t)
        .unwrap_or(inner)
        .trim();
    let target = target.strip_suffix(".md").unwrap_or(target);
    if target.is_empty() {
        None
    } else {
        Some(target.to_string())
    }
}

/// Read just the `name` frontmatter field from a markdown file on disk,
/// parsing its YAML frontmatter block directly. Returns `None` on any I/O or
/// parse failure, or when there is no `name`. Self-contained (does not depend
/// on the rest of the parser, whose body may be unimplemented) and resilient by
/// design.
fn read_frontmatter_name(abs: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(abs).ok()?;
    let yaml = frontmatter_block(&text)?;
    let value: Value = serde_yml::from_str(&yaml).ok()?;
    // `&str` indexes a YAML mapping; `None` for non-mappings or absent `name`.
    value.get("name")?.as_str().map(str::to_string)
}

/// Slice out the YAML frontmatter block (text between a leading `---` line and
/// the next `---` line). `None` if the file doesn't open with a `---` fence.
fn frontmatter_block(text: &str) -> Option<String> {
    let mut lines = text.lines();
    // First non-empty content must be the opening fence (allow a leading BOM).
    let first = lines.next()?.trim_start_matches('\u{feff}').trim_end();
    if first != "---" {
        return None;
    }
    let mut block = String::new();
    for line in lines {
        if line.trim_end() == "---" {
            return Some(block);
        }
        block.push_str(line);
        block.push('\n');
    }
    // No closing fence.
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Config;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    // ── Fixtures ─────────────────────────────────────────────────────────────

    /// A temp store: a `DB.md` marker at the root, returned alongside an opened
    /// [`Store`] handle. The handle is built directly (not via `Store::open`) so
    /// these tests exercise the `summary` code under test, not store-open
    /// plumbing.
    struct Fixture {
        _tmp: TempDir,
        store: Store,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = TempDir::new().expect("tempdir");
            let root = tmp.path().to_path_buf();
            fs::write(root.join("DB.md"), "---\ntype: db-md\n---\n").expect("write DB.md");
            let store = Store {
                root,
                config: Config::default(),
            };
            Fixture { _tmp: tmp, store }
        }

        /// Write a company record with the given store-relative path (no `.md`)
        /// and `name`, so `compose_contact` can resolve it.
        fn write_company(&self, rel_no_ext: &str, name: &str) {
            let mut path: PathBuf = self.store.root.join(rel_no_ext);
            path.set_extension("md");
            fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
            let contents =
                format!("---\ntype: company\nname: {name}\nindustry: SaaS\n---\n\n# {name}\n");
            fs::write(path, contents).expect("write company");
        }
    }

    /// Build a [`Frontmatter`] from a YAML map literal so tests state intent in
    /// YAML, not by hand-poking `extra`. This goes through `serde_yml` exactly
    /// like a real file's frontmatter would.
    fn fm(yaml: &str) -> Frontmatter {
        let value: Value = serde_yml::from_str(yaml).expect("test yaml parses");
        let mapping = value.as_mapping().expect("test yaml is a mapping").clone();
        let mut f = Frontmatter::default();
        for (k, v) in mapping {
            let key = k.as_str().expect("string key").to_string();
            match key.as_str() {
                "type" => f.type_ = v.as_str().map(str::to_string),
                "summary" => f.summary = v.as_str().map(str::to_string),
                "id" => f.id = v.as_str().map(str::to_string),
                "status" => f.status = v.as_str().map(str::to_string),
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

    // ── contact ──────────────────────────────────────────────────────────────

    #[test]
    fn contact_resolves_company_link_to_company_name() {
        let fx = Fixture::new();
        fx.write_company("records/companies/northstar", "Northstar Logistics");
        let f = fm("type: contact\n\
             role: Director of Operations\n\
             company: \"[[records/companies/northstar]]\"\n\
             last_touch: 2026-05-22\n");
        let got = compose_default(&fx.store, "contact", &f, "").unwrap();
        assert_eq!(
            got,
            "Director of Operations at Northstar Logistics (last_touch: 2026-05-22)"
        );
    }

    #[test]
    fn contact_falls_back_to_link_leaf_when_company_file_missing() {
        let fx = Fixture::new();
        // No company file written → resolution must degrade, not error.
        let f = fm("type: contact\n\
             role: VP Sales\n\
             company: \"[[records/companies/acme-corp]]\"\n\
             last_touch: 2026-01-02\n");
        let got = compose_default(&fx.store, "contact", &f, "").unwrap();
        // Falls back to the link's leaf segment, NOT the raw [[...]] form.
        assert_eq!(got, "VP Sales at acme-corp (last_touch: 2026-01-02)");
        assert!(!got.contains("[["));
    }

    #[test]
    fn contact_prefers_link_display_override_on_fallback() {
        let fx = Fixture::new();
        let f = fm("type: contact\n\
             role: Founder\n\
             company: \"[[records/companies/acme|Acme Inc]]\"\n");
        let got = compose_contact(&fx.store, &f).unwrap();
        // No last_touch → parenthetical dropped; display override used.
        assert_eq!(got, "Founder at Acme Inc");
    }

    #[test]
    fn contact_drops_company_segment_when_absent() {
        let fx = Fixture::new();
        let f = fm("type: contact\nrole: Advisor\nlast_touch: 2026-03-03\n");
        let got = compose_contact(&fx.store, &f).unwrap();
        assert_eq!(got, "Advisor (last_touch: 2026-03-03)");
    }

    #[test]
    fn contact_uses_placeholder_when_role_absent() {
        let fx = Fixture::new();
        fx.write_company("records/companies/northstar", "Northstar");
        let f = fm("type: contact\n\
             company: \"[[records/companies/northstar]]\"\n");
        let got = compose_contact(&fx.store, &f).unwrap();
        assert_eq!(got, "Contact at Northstar");
    }

    // ── company ──────────────────────────────────────────────────────────────

    #[test]
    fn company_joins_relationship_and_industry() {
        let f = fm("type: company\nrelationship: customer\nindustry: Logistics\n");
        assert_eq!(compose_company(&f), "customer; Logistics");
    }

    #[test]
    fn company_drops_separator_when_one_field_missing() {
        let f = fm("type: company\nrelationship: vendor\n");
        assert_eq!(compose_company(&f), "vendor");
        let f2 = fm("type: company\nindustry: Fintech\n");
        assert_eq!(compose_company(&f2), "Fintech");
    }

    // ── expense ──────────────────────────────────────────────────────────────

    #[test]
    fn expense_formats_date_amount_currency_vendor() {
        let f = fm("type: expense\n\
             date: 2026-04-01\n\
             amount: 49.99\n\
             currency: USD\n\
             vendor: GitHub\n");
        assert_eq!(compose_expense(&f), "2026-04-01 — 49.99 USD — GitHub");
    }

    #[test]
    fn expense_renders_integer_amount_without_trailing_zero() {
        let f = fm("type: expense\ndate: 2026-04-01\namount: 50\ncurrency: EUR\nvendor: AWS\n");
        // 50 must not become 50.0.
        assert_eq!(compose_expense(&f), "2026-04-01 — 50 EUR — AWS");
    }

    #[test]
    fn expense_drops_missing_segments() {
        let f = fm("type: expense\namount: 12\ncurrency: USD\n");
        assert_eq!(compose_expense(&f), "12 USD");
    }

    // ── meeting ──────────────────────────────────────────────────────────────

    #[test]
    fn meeting_lists_first_three_attendees_with_more_count() {
        let f = fm("type: meeting\n\
             date: 2026-05-10\n\
             attendees:\n\
             \x20 - \"[[records/contacts/alice]]\"\n\
             \x20 - \"[[records/contacts/bob]]\"\n\
             \x20 - \"[[records/contacts/carol]]\"\n\
             \x20 - \"[[records/contacts/dave]]\"\n\
             \x20 - \"[[records/contacts/erin]]\"\n");
        let got = compose_meeting(&f);
        assert_eq!(got, "2026-05-10 — alice, bob, carol (+2 more)");
    }

    #[test]
    fn meeting_omits_more_suffix_at_three_or_fewer() {
        let f = fm("type: meeting\n\
             date: 2026-05-10\n\
             attendees:\n\
             \x20 - \"[[records/contacts/alice]]\"\n\
             \x20 - \"[[records/contacts/bob]]\"\n");
        assert_eq!(compose_meeting(&f), "2026-05-10 — alice, bob");
    }

    #[test]
    fn meeting_with_only_date_has_no_dash() {
        let f = fm("type: meeting\ndate: 2026-05-10\n");
        assert_eq!(compose_meeting(&f), "2026-05-10");
    }

    // ── decision ─────────────────────────────────────────────────────────────

    #[test]
    fn decision_uses_decided_by_and_first_heading() {
        let f = fm("type: decision\ndecided_by: Carlos\n");
        let body = "# Adopt Postgres over MySQL\n\nWe chose Postgres for JSONB.\n";
        assert_eq!(
            compose_decision(&f, body),
            "Carlos: Adopt Postgres over MySQL"
        );
    }

    #[test]
    fn decision_falls_back_to_first_paragraph_without_heading() {
        let f = fm("type: decision\ndecided_by: Board\n");
        let body = "Ship the v2 pricing on June 1.\n";
        assert_eq!(
            compose_decision(&f, body),
            "Board: Ship the v2 pricing on June 1."
        );
    }

    #[test]
    fn decision_strips_heading_hashes_at_any_depth() {
        let f = fm("type: decision\ndecided_by: Eng\n");
        let body = "### Use feature flags for the rollout\n";
        assert_eq!(
            compose_decision(&f, body),
            "Eng: Use feature flags for the rollout"
        );
    }

    // ── invoice ──────────────────────────────────────────────────────────────

    #[test]
    fn invoice_formats_vendor_amount_status() {
        let f = fm("type: invoice\nvendor: Acme\namount: 1200\nstatus: paid\n");
        assert_eq!(compose_invoice(&f), "Acme — 1200 — paid");
    }

    // ── email ────────────────────────────────────────────────────────────────

    #[test]
    fn email_formats_from_arrow_to_subject() {
        let f = fm("type: email\n\
             from: sarah@northstar.io\n\
             to: carlos@example.com\n\
             subject: Renewal terms\n");
        assert_eq!(
            compose_email(&f),
            "sarah@northstar.io → carlos@example.com — Renewal terms"
        );
    }

    #[test]
    fn email_joins_multiple_recipients() {
        let f = fm("type: email\n\
             from: a@x.com\n\
             to:\n\
             \x20 - b@y.com\n\
             \x20 - c@z.com\n\
             subject: Kickoff\n");
        assert_eq!(compose_email(&f), "a@x.com → b@y.com, c@z.com — Kickoff");
    }

    // ── transcript ───────────────────────────────────────────────────────────

    #[test]
    fn transcript_formats_recorded_at_and_attendees() {
        let f = fm("type: transcript\n\
             recorded_at: 2026-02-14T09:00:00-08:00\n\
             attendees:\n\
             \x20 - Alice\n\
             \x20 - Bob\n");
        assert_eq!(
            compose_transcript(&f),
            "2026-02-14T09:00:00-08:00 — Alice, Bob"
        );
    }

    // ── pdf-source ───────────────────────────────────────────────────────────

    #[test]
    fn pdf_source_formats_doc_type_from_received_from() {
        let f = fm("type: pdf-source\ndoc_type: contract\nreceived_from: Northstar Legal\n");
        assert_eq!(compose_pdf_source(&f), "contract from Northstar Legal");
    }

    // ── wiki-page ────────────────────────────────────────────────────────────

    #[test]
    fn wiki_page_prefers_topic_field() {
        let f = fm("type: wiki-page\ntopic: Renewal strategy\n");
        let body = "# Renewal strategy\n\nLots of detail here.\n";
        // topic wins over body paragraph.
        assert_eq!(
            compose_default(&Fixture::new().store, "wiki-page", &f, body).unwrap(),
            "Renewal strategy"
        );
    }

    #[test]
    fn wiki_page_falls_back_to_first_paragraph_without_topic() {
        let f = fm("type: wiki-page\n");
        let body = "# Heading skipped\n\nThe synthesis of our pricing decisions.\n";
        assert_eq!(
            compose_wiki_page(&f, body),
            "The synthesis of our pricing decisions."
        );
    }

    // ── unknown / custom + body extraction ─────────────────────────────────────

    #[test]
    fn unknown_type_uses_first_non_heading_paragraph() {
        let fx = Fixture::new();
        let f = fm("type: proposal\n");
        let body = "# Title\n\nThis proposal covers the Q3 roadmap.\n\nSecond paragraph.\n";
        let got = compose_default(&fx.store, "proposal", &f, body).unwrap();
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
        let fx = Fixture::new();
        let f = fm("type: note\n");
        let long = "word ".repeat(100); // 500 chars
        let got = compose_default(&fx.store, "note", &f, &long).unwrap();
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

    // ── determinism ────────────────────────────────────────────────────────────

    #[test]
    fn compose_default_is_deterministic_across_calls() {
        let fx = Fixture::new();
        fx.write_company("records/companies/northstar", "Northstar");
        let f = fm("type: contact\n\
             role: Ops Lead\n\
             company: \"[[records/companies/northstar]]\"\n\
             last_touch: 2026-05-22\n");
        let a = compose_default(&fx.store, "contact", &f, "body").unwrap();
        let b = compose_default(&fx.store, "contact", &f, "body").unwrap();
        let c = compose_default(&fx.store, "contact", &f, "body").unwrap();
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn empty_frontmatter_company_yields_empty_summary() {
        // No relationship/industry → empty (validate later flags SUMMARY_EMPTY).
        let f = fm("type: company\n");
        assert_eq!(
            compose_default(&Fixture::new().store, "company", &f, "").unwrap(),
            ""
        );
    }
}
