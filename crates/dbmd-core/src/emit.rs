// SPDX-License-Identifier: Apache-2.0

//! `emit` — the whole-store structured dump (a SWEEP, off the loop).
//!
//! [`compute`] walks every content file (`sources/` + `records/`, per the
//! same [`Store::walk`] discovery every SWEEP uses — derived `index.md`
//! catalogs are skipped) plus the root `DB.md`, and projects each into an
//! [`EmittedFile`]: the parsed frontmatter with values verbatim, the derived
//! fields (layer, `type`, effective `meta-type`, title, `summary`,
//! timestamps), the verbatim body, the normalized wiki-link targets, and the
//! SHA-256 of the raw file bytes. The host-integration surface: a hub, an
//! indexer, or a migration ingests a store as a pure consumer of `dbmd`
//! output instead of reimplementing the parse.
//!
//! **Lenient by design.** A dump must describe the store as it is, so a
//! malformed file degrades instead of aborting the sweep: a file with no
//! frontmatter block emits an empty `frontmatter` with the whole text as
//! `body`; unparseable frontmatter YAML emits an empty `frontmatter` with the
//! after-fence remainder as `body`; a bad `created`/`updated` scalar leaves
//! the typed timestamp unset while the raw value still rides in
//! `frontmatter`. (Reporting those defects is `validate`'s job, not the
//! dump's.) Only real failures — an unreadable file, a broken walk — error.
//!
//! **One notion, shared with the rest of the toolkit.** Link extraction is
//! [`store::extract_edge_targets`] (fence-aware, alias-stripped,
//! whitespace-trimmed) with the `.md` extension appended — the same
//! resolution `graph` applies, in the on-disk spelling a consumer can match
//! against `path` directly. Scalar coercion and the YAML→JSON value
//! projection are the `index` module's ([`crate::index`]), so `emit` and
//! `query --json` present identical value shapes. The effective `meta-type`
//! mirrors [`Frontmatter::effective_meta_type`]: records only, absent ⇒
//! `fact`, declared values verbatim. Title derivation reuses the `render`
//! module's CommonMark ATX heading rules.
//!
//! [`Frontmatter::effective_meta_type`]: crate::parser::Frontmatter::effective_meta_type

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset};
use sha2::{Digest, Sha256};

use crate::index::{parse_ts, scalar_string, yaml_to_json_value};
use crate::parser::split_frontmatter;
use crate::render::{heading_level, heading_text};
use crate::store::{self, EdgeSpan, Layer, Store};

/// One file of the dump: the store-relative identity, the parsed frontmatter
/// (values verbatim), the derived fields, the verbatim body, the normalized
/// link targets, and the content hash.
#[derive(Debug, Clone, PartialEq)]
pub struct EmittedFile {
    /// Store-relative path, POSIX separators (`records/contacts/sarah.md`).
    pub path: String,
    /// The layer the file lives in; `None` for the root `DB.md`.
    pub layer: Option<Layer>,
    /// The full parsed frontmatter mapping, values verbatim (the `index`
    /// projection: strings/numbers/bools/lists as written; an inline
    /// `[[...]]`-valued field as its wiki-link literal). Empty when the file
    /// has no frontmatter block or its YAML does not parse.
    pub frontmatter: serde_json::Map<String, serde_json::Value>,
    /// The frontmatter `type`, scalar-coerced like `index`/`validate` coerce it.
    pub type_: Option<String>,
    /// The effective `meta-type` — records only: the declared value verbatim,
    /// or `fact` when absent (SPEC default). `None` for sources and `DB.md`.
    pub meta_type: Option<String>,
    /// Display title: the `name` field, else the `title` field, else the
    /// body's first ATX `#` heading (fence-aware, CommonMark rules).
    pub title: Option<String>,
    /// The frontmatter `summary`, scalar-coerced; `None` when absent.
    pub summary: Option<String>,
    /// The verbatim markdown body after the frontmatter block (the whole text
    /// when the file has no frontmatter block).
    pub body: String,
    /// Normalized wiki-link targets in first-appearance order, deduped:
    /// alias stripped (text before `|`), whitespace trimmed, `.md` appended —
    /// the on-disk spelling, so a target matches a document `path` directly.
    /// Dangling targets are included (existence is `validate`'s concern).
    pub links: Vec<String>,
    /// Every wiki-link OCCURRENCE in the body, in document order, with the byte
    /// span it covers in `body` — the positional view `links` cannot give
    /// (`links` is a deduped set; a renderer needs to splice at offsets).
    ///
    /// Body-only, deliberately: a `[[…]]` in a frontmatter VALUE is a real edge
    /// (and appears in `links`) but is field data, never markdown rendered in
    /// place, so it has no span. Empty for `DB.md` and for bodies with no
    /// links.
    pub link_spans: Vec<EdgeSpan>,
    /// Frontmatter `created`, when present and RFC3339-parseable.
    pub created: Option<DateTime<FixedOffset>>,
    /// Frontmatter `updated`, when present and RFC3339-parseable.
    pub updated: Option<DateTime<FixedOffset>>,
    /// Lowercase-hex SHA-256 of the raw file bytes — the exact bytes this
    /// projection was parsed from, so a consumer can detect drift.
    pub sha256: String,
}

/// A computed whole-store dump: every emitted file plus the per-layer tally.
#[derive(Debug, Clone, PartialEq)]
pub struct Emit {
    /// Every emitted file (content files + `DB.md`), sorted by path.
    pub files: Vec<EmittedFile>,
    /// How many emitted files live in `sources/`.
    pub sources: usize,
    /// How many emitted files live in `records/`.
    pub records: usize,
}

/// **SWEEP.** Project the whole store into an [`Emit`]: every content file
/// via [`Store::walk`] (both layers, derived catalogs skipped) plus the root
/// `DB.md`, sorted by path. Read-only; errors only on real failures (an
/// unreadable file, a broken walk) — malformed content degrades per the
/// module contract, it never aborts the dump.
pub fn compute(store: &Store) -> crate::Result<Emit> {
    let mut rels: Vec<PathBuf> = store.walk()?;
    rels.push(PathBuf::from("DB.md"));
    rels.sort();

    let mut files = Vec::with_capacity(rels.len());
    let mut sources = 0usize;
    let mut records = 0usize;
    for rel in &rels {
        let file = emit_file(store, rel)?;
        match file.layer {
            Some(Layer::Sources) => sources += 1,
            Some(Layer::Records) => records += 1,
            None => {}
        }
        files.push(file);
    }
    Ok(Emit {
        files,
        sources,
        records,
    })
}

/// Project one store-relative file into its [`EmittedFile`].
fn emit_file(store: &Store, rel: &Path) -> crate::Result<EmittedFile> {
    let abs = store.abs_path(rel);
    let bytes = std::fs::read(&abs)?;
    let sha256 = sha256_hex(&bytes);

    // Decode lossily: `sources/` is preserved verbatim per the SPEC and can
    // carry non-UTF-8 imports; a stray byte substitutes U+FFFD rather than
    // aborting the sweep (the same posture as the index projection and the
    // store's link scan).
    let text = String::from_utf8_lossy(&bytes);

    // Split the frontmatter block with the canonical splitter (BOM + fence
    // tolerance identical to every write surface). A file with no block — or
    // an unterminated one — is still a complete dump member: empty
    // frontmatter, the whole text as body.
    let (yaml, body) = match split_frontmatter(&text, &abs) {
        Ok(parsed) => (parsed.frontmatter_yaml, parsed.body),
        Err(_) => (String::new(), text.clone().into_owned()),
    };

    // Parse the frontmatter YAML leniently: a malformed mapping yields an
    // empty frontmatter (the body still carries the file), mirroring how a
    // hand-written store degrades. Non-string keys are skipped, matching the
    // index projection.
    let map: serde_norway::Mapping = if yaml.trim().is_empty() {
        serde_norway::Mapping::new()
    } else {
        serde_norway::from_str(&yaml).unwrap_or_default()
    };

    let mut frontmatter = serde_json::Map::new();
    let mut type_ = None;
    let mut summary = None;
    let mut declared_meta_type = None;
    let mut name_field = None;
    let mut title_field = None;
    let mut created = None;
    let mut updated = None;
    for (k, v) in &map {
        let Some(key) = k.as_str() else { continue };
        match key {
            "type" => type_ = scalar_string(v),
            "summary" => summary = scalar_string(v),
            "meta-type" => declared_meta_type = scalar_string(v),
            "name" => name_field = non_empty(scalar_string(v)),
            "title" => title_field = non_empty(scalar_string(v)),
            "created" => created = v.as_str().and_then(parse_ts),
            "updated" => updated = v.as_str().and_then(parse_ts),
            _ => {}
        }
        frontmatter.insert(key.to_string(), yaml_to_json_value(v));
    }

    let layer = rel
        .components()
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .and_then(Layer::from_dir_name);

    // Effective meta-type: records only; declared verbatim, absent ⇒ `fact`
    // (`Frontmatter::effective_meta_type` / the index projection's default).
    let meta_type = match layer {
        Some(Layer::Records) => Some(declared_meta_type.unwrap_or_else(|| "fact".to_string())),
        _ => None,
    };

    let title = name_field.or(title_field).or_else(|| first_h1(&body));

    // Wiki-link targets over the WHOLE text (frontmatter values + body — the
    // shared edge extractor handles the split and the fence state), `.md`
    // appended to the canonical form, deduped in first-appearance order. The
    // dedup key is the canonical spelling verbatim (byte-portable across
    // hosts) — the local filesystem's case folding is a resolution concern,
    // not a dump concern.
    let mut links = Vec::new();
    let mut seen = BTreeSet::new();
    for target in store::extract_edge_targets(&text) {
        let with_md = format!("{target}.md");
        if seen.insert(with_md.clone()) {
            links.push(with_md);
        }
    }

    // Positional occurrences over the BODY only (see the field docs). The
    // shared extractor guarantees these agree with `links` on every fence
    // decision — one grammar, two views.
    let link_spans = store::extract_edge_spans(&body);

    Ok(EmittedFile {
        path: rel.to_string_lossy().replace('\\', "/"),
        layer,
        frontmatter,
        type_,
        meta_type,
        title,
        summary,
        body,
        links,
        link_spans,
        created,
        updated,
        sha256,
    })
}

/// A trimmed, non-empty scalar; `None` otherwise. The `name`/`title` fields
/// only count as a display title when they carry visible text.
fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// The body's first ATX `#` (level-1) heading text, fence-aware: a `# ...`
/// line inside a ``` / `~~~` fenced code block is code, not a title. Heading
/// recognition and text extraction are the `render` module's CommonMark rules
/// ([`heading_level`] / [`heading_text`]), so the dump's title agrees with
/// `dbmd sections` / `dbmd outline` on what a heading is. An empty heading
/// (`#` alone, `# ##`) yields no title and the scan continues.
fn first_h1(body: &str) -> Option<String> {
    let mut fence: Option<(u8, usize)> = None;
    for line in body.lines() {
        let content = line.trim_end_matches('\r');
        if let Some(f) = fence {
            if store::fence_closes(content, f) {
                fence = None;
            }
            continue;
        }
        if let Some(opened) = store::fence_opens(content) {
            fence = Some(opened);
            continue;
        }
        if heading_level(content) == 1 {
            let text = heading_text(content, 1);
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    None
}

/// Lowercase-hex SHA-256 of `bytes` — hashed over the same in-memory bytes
/// the projection parsed, so the digest and the emitted content can never
/// disagree about which file version was read.
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for b in digest.iter() {
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway store rooted in a tempdir, with a `DB.md` marker.
    fn store() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(
            tmp.path().join("DB.md"),
            "---\ntype: db-md\nscope: test\n---\n\n# Test store\n",
        )
        .expect("DB.md");
        let store = Store::open_strict(tmp.path()).expect("open store");
        (tmp, store)
    }

    fn seed(root: &Path, rel: &str, contents: &str) {
        let abs = root.join(rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(abs, contents).unwrap();
    }

    fn by_path<'a>(emit: &'a Emit, path: &str) -> &'a EmittedFile {
        emit.files
            .iter()
            .find(|f| f.path == path)
            .unwrap_or_else(|| panic!("no emitted file {path}"))
    }

    #[test]
    fn title_prefers_name_then_title_then_first_h1() {
        let (tmp, store) = store();
        seed(
            tmp.path(),
            "records/contacts/named.md",
            "---\ntype: contact\nname: Sarah Chen\ntitle: Ignored\nsummary: s\n---\n\n# Also ignored\n",
        );
        seed(
            tmp.path(),
            "records/contacts/titled.md",
            "---\ntype: contact\ntitle: The Title\nsummary: s\n---\nbody\n",
        );
        seed(
            tmp.path(),
            "records/decisions/h1.md",
            "---\ntype: decision\nsummary: s\n---\n\n```\n# fenced, not a title\n```\n\n# Real Title ##\n",
        );
        let emit = compute(&store).expect("emit");
        assert_eq!(
            by_path(&emit, "records/contacts/named.md").title.as_deref(),
            Some("Sarah Chen")
        );
        assert_eq!(
            by_path(&emit, "records/contacts/titled.md")
                .title
                .as_deref(),
            Some("The Title")
        );
        // Fence-aware: the fenced `#` line is code; the real H1's closing-hash
        // run is stripped per the CommonMark ATX rule.
        assert_eq!(
            by_path(&emit, "records/decisions/h1.md").title.as_deref(),
            Some("Real Title")
        );
    }

    #[test]
    fn no_frontmatter_degrades_to_empty_frontmatter_and_whole_body() {
        let (tmp, store) = store();
        let text = "Just a plain note, no frontmatter.\n";
        seed(tmp.path(), "sources/notes/plain.md", text);
        let emit = compute(&store).expect("emit");
        let f = by_path(&emit, "sources/notes/plain.md");
        assert!(f.frontmatter.is_empty());
        assert_eq!(f.type_, None);
        assert_eq!(f.body, text);
        assert_eq!(f.layer, Some(Layer::Sources));
    }

    #[test]
    fn meta_type_defaults_for_records_only() {
        let (tmp, store) = store();
        seed(
            tmp.path(),
            "records/contacts/fact.md",
            "---\ntype: contact\nsummary: s\n---\nbody\n",
        );
        seed(
            tmp.path(),
            "records/decisions/conclusion.md",
            "---\ntype: decision\nmeta-type: conclusion\nsummary: s\n---\nbody\n",
        );
        seed(
            tmp.path(),
            "sources/notes/n.md",
            "---\ntype: note\nsummary: s\n---\nbody\n",
        );
        let emit = compute(&store).expect("emit");
        assert_eq!(
            by_path(&emit, "records/contacts/fact.md")
                .meta_type
                .as_deref(),
            Some("fact")
        );
        assert_eq!(
            by_path(&emit, "records/decisions/conclusion.md")
                .meta_type
                .as_deref(),
            Some("conclusion")
        );
        assert_eq!(by_path(&emit, "sources/notes/n.md").meta_type, None);
        assert_eq!(by_path(&emit, "DB.md").meta_type, None);
    }

    #[test]
    fn links_are_normalized_deduped_and_fence_aware() {
        let (tmp, store) = store();
        seed(
            tmp.path(),
            "sources/notes/n.md",
            "---\ntype: note\nsummary: s\ncompany: \"[[records/companies/acme]]\"\n---\n\
             See [[records/contacts/sarah]] and [[records/contacts/sarah.md|Sarah]].\n\
             Dangling: [[records/ghosts/nobody]].\n\
             ```\n[[records/contacts/fenced]]\n```\n",
        );
        let emit = compute(&store).expect("emit");
        let f = by_path(&emit, "sources/notes/n.md");
        // Frontmatter link first (extraction order), then body links in
        // first-appearance order; the `.md` and bare spellings collapse; the
        // fenced pseudo-link is code, not an edge; the dangling target stays.
        assert_eq!(
            f.links,
            vec![
                "records/companies/acme.md".to_string(),
                "records/contacts/sarah.md".to_string(),
                "records/ghosts/nobody.md".to_string(),
            ]
        );
    }

    #[test]
    fn db_md_is_emitted_with_no_layer_and_counts_ride_the_layers() {
        let (tmp, store) = store();
        seed(
            tmp.path(),
            "sources/notes/n.md",
            "---\ntype: note\nsummary: s\n---\nbody\n",
        );
        seed(
            tmp.path(),
            "records/contacts/c.md",
            "---\ntype: contact\nsummary: s\n---\nbody\n",
        );
        // A derived catalog must not be emitted.
        seed(
            tmp.path(),
            "records/contacts/index.md",
            "# Contacts index\n",
        );
        let emit = compute(&store).expect("emit");
        let paths: Vec<&str> = emit.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["DB.md", "records/contacts/c.md", "sources/notes/n.md"]
        );
        let db = by_path(&emit, "DB.md");
        assert_eq!(db.layer, None);
        assert_eq!(db.type_.as_deref(), Some("db-md"));
        assert_eq!(db.title.as_deref(), Some("Test store"));
        assert_eq!((emit.files.len(), emit.sources, emit.records), (3, 1, 1));
    }
}
