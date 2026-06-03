//! `parser` — read and write db.md markdown files.
//!
//! Parses the YAML frontmatter block, the markdown body, wiki-links, standard
//! markdown links, `##` sections, and the structured sections of the `DB.md`
//! config file. Also the atomic writer that round-trips a file while
//! preserving the operator-edited body verbatim and emitting frontmatter in
//! canonical key order.
//!
//! Strict on required fields, lenient on unknowns: any frontmatter key the
//! spec doesn't recognize is preserved in [`Frontmatter::extra`] as ambient
//! context and round-tripped untouched.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset};
use serde_norway::{Mapping, Value};

/// The three canonical layer folder names. A path is "content" / a wiki-link is
/// "full-path" only when it resolves under one of these.
const LAYER_DIRS: [&str; 3] = ["sources", "records", "wiki"];

/// Errors produced while parsing a markdown file or the `DB.md` config.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// The frontmatter block was not valid YAML. Maps to validate code
    /// `FM_MALFORMED_YAML`.
    #[error("malformed YAML frontmatter in {file}: {source}")]
    MalformedYaml {
        /// The file whose frontmatter failed to parse.
        file: PathBuf,
        /// The underlying YAML error.
        source: serde_norway::Error,
    },

    /// The file has no `---`-delimited frontmatter block at its very start.
    #[error("missing frontmatter block in {file}")]
    MissingFrontmatter {
        /// The offending file.
        file: PathBuf,
    },

    /// A required field was absent. Maps to validate code `FM_MISSING_TYPE`
    /// (for `type`) and the per-type required-field codes.
    #[error("missing required field '{key}' in {file}")]
    MissingField {
        /// The file missing the field.
        file: PathBuf,
        /// The required key.
        key: String,
    },

    /// A timestamp field was not ISO-8601 / RFC3339. Maps to `FM_BAD_TIMESTAMP`.
    #[error("bad timestamp in field '{key}' of {file}: {value}")]
    BadTimestamp {
        /// The file.
        file: PathBuf,
        /// The frontmatter key.
        key: String,
        /// The unparseable value.
        value: String,
    },

    /// An I/O error reading the file.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The parsed YAML frontmatter of a db.md file.
///
/// The universal-contract fields are typed accessors; everything else lands in
/// [`extra`](Frontmatter::extra) as ambient context (unknown-field passthrough)
/// and is round-tripped verbatim. The atomic writer re-emits keys in canonical
/// order: `type`, `id`, `created`, `updated`, `summary` first, then
/// type-specific fields, then `status` / `tags`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Frontmatter {
    /// `type` — required on content files; the primary query key.
    pub type_: Option<String>,
    /// `id` — optional; derived from the file path when absent.
    pub id: Option<String>,
    /// `created` — RFC3339; required and auto-set on content-file create.
    pub created: Option<DateTime<FixedOffset>>,
    /// `updated` — RFC3339; required and auto-maintained on content files.
    pub updated: Option<DateTime<FixedOffset>>,
    /// `summary` — the one-line catalog line; required on every content file.
    pub summary: Option<String>,
    /// `status` — optional lifecycle state.
    pub status: Option<String>,
    /// `tags` — optional flat list of short scalar labels.
    pub tags: Vec<String>,
    /// All other frontmatter keys (type-specific + custom), preserved verbatim
    /// in insertion-stable sorted order. Wiki-link-valued fields keep their raw
    /// YAML form here; [`Frontmatter::link_fields`] surfaces them as
    /// [`WikiLink`]s.
    pub extra: BTreeMap<String, Value>,
}

impl Frontmatter {
    /// Parse a YAML frontmatter block (the text between the opening and closing
    /// `---` fences, exclusive) into a [`Frontmatter`].
    ///
    /// Lenient on unknown keys (they go to [`extra`](Frontmatter::extra));
    /// returns [`ParseError::MalformedYaml`] only on YAML that doesn't parse.
    pub fn parse(yaml: &str, file: &Path) -> Result<Self, ParseError> {
        // An empty (or whitespace-only) frontmatter block is a valid, empty
        // mapping — not a YAML error.
        let value: Value = if yaml.trim().is_empty() {
            Value::Mapping(Mapping::new())
        } else {
            serde_norway::from_str(yaml).map_err(|source| ParseError::MalformedYaml {
                file: file.to_path_buf(),
                source,
            })?
        };

        // Top-level frontmatter must be a mapping. A scalar or sequence at the
        // top level is malformed for our purposes; surface it as such.
        let map = match value {
            Value::Mapping(m) => m,
            Value::Null => Mapping::new(),
            other => {
                // serde_norway::Error has no public constructor, so let the
                // deserializer decide: a value that coerces to a Mapping (e.g. a
                // YAML-tagged mapping `!tag\n k: v`, where the tag is ambient) is
                // accepted as that mapping; a genuine scalar or sequence top
                // level fails to coerce and IS the malformed case. (Using a
                // match here, not `expect_err`, avoids a panic on the
                // tagged-mapping case, which deserializes to a Mapping just
                // fine.)
                match serde_norway::from_value::<Mapping>(other) {
                    Ok(m) => m,
                    Err(source) => {
                        return Err(ParseError::MalformedYaml {
                            file: file.to_path_buf(),
                            source,
                        });
                    }
                }
            }
        };

        let mut fm = Frontmatter::default();
        for (k, v) in map {
            let key = match k.as_str() {
                Some(s) => s.to_string(),
                // Non-string keys are unusual; stringify defensively and keep
                // them in `extra` so nothing is silently dropped.
                None => format!("{k:?}"),
            };
            match key.as_str() {
                "type" => fm.type_ = v.as_str().map(str::to_string),
                "id" => fm.id = v.as_str().map(str::to_string),
                "created" => fm.created = parse_timestamp(&v, "created", file)?,
                "updated" => fm.updated = parse_timestamp(&v, "updated", file)?,
                "summary" => fm.summary = v.as_str().map(str::to_string),
                "status" => fm.status = v.as_str().map(str::to_string),
                "tags" => fm.tags = parse_tags(&v),
                _ => {
                    fm.extra.insert(key, v);
                }
            }
        }
        Ok(fm)
    }

    /// Serialize the frontmatter back to a YAML block (no `---` fences) in
    /// canonical key order. Round-trips [`extra`](Frontmatter::extra) verbatim.
    pub fn to_yaml(&self) -> String {
        // Build an order-preserving mapping in canonical key order:
        //   type, id, created, updated, summary  (universal head)
        //   <type-specific extra, BTreeMap-sorted>
        //   status, tags                          (universal tail)
        // serde_norway::Mapping preserves insertion order, so one serialize call
        // emits the block in exactly this order with correct YAML quoting.
        let mut map = Mapping::new();

        if let Some(t) = &self.type_ {
            map.insert(Value::String("type".into()), Value::String(t.clone()));
        }
        if let Some(id) = &self.id {
            map.insert(Value::String("id".into()), Value::String(id.clone()));
        }
        if let Some(created) = &self.created {
            map.insert(
                Value::String("created".into()),
                Value::String(created.to_rfc3339()),
            );
        }
        if let Some(updated) = &self.updated {
            map.insert(
                Value::String("updated".into()),
                Value::String(updated.to_rfc3339()),
            );
        }
        if let Some(summary) = &self.summary {
            map.insert(
                Value::String("summary".into()),
                Value::String(summary.clone()),
            );
        }

        // Type-specific + custom fields, in BTreeMap (sorted) order. Each value
        // is canonicalized so a wiki-link round-trips to the form the writer and
        // `dbmd validate` agree on — critically, the SPEC-canonical *unquoted*
        // scalar `field: [[x]]` (which YAML parses to a nested `Seq[Seq[String]]`)
        // is re-emitted as a quoted scalar `'[[x]]'` instead of the bracket-less
        // block sequence `- - x` that a verbatim re-emit would produce and that
        // destroys the link. See [`canonicalize_extra_value`].
        for (k, v) in &self.extra {
            map.insert(Value::String(k.clone()), canonicalize_extra_value(v));
        }

        if let Some(status) = &self.status {
            map.insert(
                Value::String("status".into()),
                Value::String(status.clone()),
            );
        }
        if !self.tags.is_empty() {
            map.insert(
                Value::String("tags".into()),
                Value::Sequence(self.tags.iter().cloned().map(Value::String).collect()),
            );
        }

        if map.is_empty() {
            return String::new();
        }
        serde_norway::to_string(&Value::Mapping(map)).unwrap_or_default()
    }

    /// True if the file is content (under `sources/`, `records/`, or `wiki/`)
    /// and not an `index.md`. Used by validate to decide which files require a
    /// `summary`. Meta files (`DB.md`, `index.md`, `log.md`) return false.
    pub fn is_content_file(path: &Path) -> bool {
        // index.md is a meta file at every level, never content.
        if path.file_name().and_then(|n| n.to_str()) == Some("index.md") {
            return false;
        }
        // Content iff some path component is one of the three layer dirs. This
        // works for both store-relative (`sources/emails/x.md`) and absolute
        // (`/home/db/sources/emails/x.md`) paths. DB.md / log.md sit at the
        // root, under no layer, so they fall through to false.
        path.components().any(|c| {
            c.as_os_str()
                .to_str()
                .is_some_and(|s| LAYER_DIRS.contains(&s))
        })
    }

    /// Resolve the file's effective `id`: the explicit `id` field if present,
    /// otherwise derived from the store-relative path (filename without `.md`).
    pub fn effective_id(&self, store_relative_path: &Path) -> String {
        if let Some(id) = &self.id {
            if !id.is_empty() {
                return id.clone();
            }
        }
        // Derived id = filename without the `.md` extension.
        store_relative_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string()
    }

    /// Read a single frontmatter key as a raw YAML [`Value`], looking in the
    /// typed fields first and then [`extra`](Frontmatter::extra).
    pub fn get(&self, key: &str) -> Option<Value> {
        match key {
            "type" => self.type_.clone().map(Value::String),
            "id" => self.id.clone().map(Value::String),
            "created" => self.created.map(|d| Value::String(d.to_rfc3339())),
            "updated" => self.updated.map(|d| Value::String(d.to_rfc3339())),
            "summary" => self.summary.clone().map(Value::String),
            "status" => self.status.clone().map(Value::String),
            "tags" => {
                if self.tags.is_empty() {
                    None
                } else {
                    Some(Value::Sequence(
                        self.tags.iter().cloned().map(Value::String).collect(),
                    ))
                }
            }
            _ => self.extra.get(key).cloned(),
        }
    }

    /// Set a single frontmatter key from a string value, routing universal-
    /// contract keys to their typed fields and everything else to
    /// [`extra`](Frontmatter::extra). Used by `dbmd fm set`.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), ParseError> {
        match key {
            "type" => self.type_ = Some(value.to_string()),
            "id" => self.id = Some(value.to_string()),
            "created" => {
                self.created = Some(parse_rfc3339(value, "created", Path::new("<fm set>"))?)
            }
            "updated" => {
                self.updated = Some(parse_rfc3339(value, "updated", Path::new("<fm set>"))?)
            }
            "summary" => self.summary = Some(value.to_string()),
            "status" => self.status = Some(value.to_string()),
            "tags" => {
                // Accept either a YAML flow list (`[a, b]`) or a single scalar
                // tag. Anything that parses to a sequence becomes the tag list;
                // otherwise the whole string is one tag.
                self.tags = match serde_norway::from_str::<Value>(value) {
                    Ok(Value::Sequence(seq)) => parse_tags(&Value::Sequence(seq)),
                    _ => vec![value.to_string()],
                };
            }
            _ => {
                // A custom / type-specific field. The value is a scalar string by
                // default, but the spec's list-valued link fields (e.g.
                // `meeting.attendees`, SPEC § Linking) must serialize as a YAML
                // block sequence of quoted wiki-links — never the flow-form string
                // `"[[[a]], [[b]]]"`, which `dbmd validate` rejects as
                // `WIKI_LINK_FLOW_FORM_LIST`. When the value parses as a YAML
                // sequence whose every item is a clean single wiki-link, store the
                // canonical sequence so `to_yaml` emits block form. Everything else
                // — plain text, and a single inline `[[x]]` (which YAML reads as a
                // nested `Seq[Seq[String]]`, not a list of link strings) — stays a
                // verbatim scalar string, preserving the prior behavior.
                let stored = parse_link_list_value(value)
                    .unwrap_or_else(|| Value::String(value.to_string()));
                self.extra.insert(key.to_string(), stored);
            }
        }
        Ok(())
    }

    /// Extract every frontmatter field whose value is a wiki-link (scalar
    /// inline form or a block-sequence list), pairing each with its key. The
    /// validate engine checks these against `(link)` schema annotations.
    pub fn link_fields(&self) -> Vec<(String, WikiLink)> {
        let mut out = Vec::new();
        // `summary` may carry navigational wiki-links (spec encourages it).
        if let Some(summary) = &self.summary {
            for link in extract_wiki_links(summary, Path::new("")) {
                out.push(("summary".to_string(), link));
            }
        }
        // Every type-specific / custom field: a scalar wiki-link or a list of
        // wiki-links, in either the quoted (`"[[x]]"`) or the canonical unquoted
        // (`[[x]]`) form. See [`links_in_field_value`] for the YAML shapes.
        for (key, value) in &self.extra {
            for link in links_in_field_value(value) {
                out.push((key.clone(), link));
            }
        }
        out
    }
}

/// A wiki-link reference inside the store: `[[target]]` or `[[target|display]]`.
///
/// `target` is always recorded as written; [`is_full_path`](WikiLink::is_full_path)
/// flags whether it's a full store-relative path (the doctrine) versus a
/// short-form (a validation error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiLink {
    /// The link target as written, without the `[[ ]]` and without `|display`.
    pub target: String,
    /// The optional `|display` text override.
    pub display: Option<String>,
    /// True when `target` is a full store-relative path (contains a `/` and
    /// resolves under a known layer); false for short-form targets like
    /// `sarah-chen` — which validate reports as `WIKI_LINK_SHORT_FORM`.
    pub is_full_path: bool,
    /// True when `target` carries a trailing `.md` extension — validate warns
    /// `WIKI_LINK_HAS_EXTENSION`; the canonical writers emit the bare form.
    pub has_md_extension: bool,
    /// Where the link appears: `(file, line, col)`, 1-based line and column.
    pub location: (PathBuf, u32, u32),
}

/// A standard markdown link `[text](url)` — an external reference, kept in a
/// stream separate from [`WikiLink`] so external targets are visible to the
/// toolkit without being conflated with in-store edges. Not graph-validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownLink {
    /// The link text inside `[ ]`.
    pub text: String,
    /// The URL or path inside `( )`.
    pub url: String,
    /// Where the link appears: `(file, line, col)`, 1-based.
    pub location: (PathBuf, u32, u32),
}

/// A `##`/`###` section of a markdown body: the heading text plus the byte
/// slice of the body it spans (heading line through the line before the next
/// heading of equal-or-shallower depth).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// The heading text (without the leading `#`s).
    pub heading: String,
    /// Heading depth (number of leading `#`s).
    pub level: u8,
    /// The 1-based line where the heading appears.
    pub line: u32,
    /// The section body, from the heading line to the next sibling-or-shallower
    /// heading (exclusive), as a slice of the original body.
    pub body: String,
}

/// The parsed structured content of a store's `DB.md` config file.
///
/// All four parts are optional in the source; absent parts fall back to spec
/// defaults. Produced by [`parse_db_md`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    /// Body of the `## Agent instructions` section — free-form prose passed to
    /// the agent's system prompt.
    pub agent_instructions: Option<String>,
    /// `## Policies` → `### Frozen pages`: store-relative paths the toolkit
    /// refuses to write (`POLICY_FROZEN_PAGE`).
    pub frozen_pages: Vec<PathBuf>,
    /// `## Policies` → `### Ignored types`: type names the curator never
    /// synthesizes (still readable as ambient context).
    pub ignored_types: Vec<String>,
    /// `## Schemas` → one entry per `### <type>` sub-section.
    pub schemas: BTreeMap<String, Schema>,
}

impl Config {
    /// The `### Frozen pages` entry that matches a store-relative `target`, if
    /// any. The **single** frozen-page matcher every write surface must funnel
    /// through so the policy is enforced identically on `write` / `fm set` /
    /// `fm init` / `link` / `rename` / `format`.
    ///
    /// Comparison is normalized so a policy line and a write target match
    /// regardless of incidental spelling differences:
    /// - `/` path separators on every OS,
    /// - a single leading `./` dropped,
    /// - a trailing `.md` dropped on **both** sides — `parse_db_md` stores
    ///   frozen entries verbatim, so an operator who writes the natural
    ///   extensionless spelling (`records/decisions/q1`) must protect the file
    ///   (`records/decisions/q1.md`) exactly as the `.md` spelling does.
    ///
    /// Returns the matched config entry verbatim (its original spelling) so the
    /// caller can name it in the `POLICY_FROZEN_PAGE` refusal.
    pub fn frozen_match(&self, target: &Path) -> Option<PathBuf> {
        let want = normalize_frozen_path(target);
        self.frozen_pages
            .iter()
            .find(|frozen| normalize_frozen_path(frozen) == want)
            .cloned()
    }

    /// True if `target` (store-relative) is a frozen page. Convenience wrapper
    /// over [`Config::frozen_match`] for callers that only need presence.
    pub fn is_frozen(&self, target: &Path) -> bool {
        self.frozen_match(target).is_some()
    }
}

/// Normalize a path for frozen-page comparison: `/` separators, a single
/// leading `./` dropped, and a trailing `.md` dropped. Both the policy entry
/// and the write target pass through this before equality, so the match is
/// separator-, `./`-, and `.md`-insensitive.
fn normalize_frozen_path(p: &Path) -> String {
    let unix: String = p
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/");
    let no_dot = unix.strip_prefix("./").unwrap_or(&unix);
    no_dot.strip_suffix(".md").unwrap_or(no_dot).to_string()
}

/// A user-declared type schema parsed from a `DB.md` `### <type>` sub-section.
/// The store's `## Schemas` is the **only** source of schema enforcement — the
/// toolkit ships no built-in or implicit per-type schema (see SPEC § Schemas).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Schema {
    /// One [`FieldSpec`] per bulleted field line, in source order.
    pub fields: Vec<FieldSpec>,
    /// `- unique: <field>[, <field> …]` directives — each inner vec is one
    /// uniqueness constraint over the listed field(s) (compound when >1). Two
    /// records of this type whose listed values collide warn as
    /// `DUP_UNIQUE_KEY`.
    pub unique_keys: Vec<Vec<String>>,
    /// `- summary_template: <template>` directive — the `{field}` interpolation
    /// pattern `dbmd fm init` / `dbmd write` use to compose a default `summary`
    /// for this type. `None` falls back to the body's first paragraph.
    pub summary_template: Option<String>,
    /// `- shard: by-date | flat` directive — whether records of this type are
    /// date-sharded on disk (`records/<type>/<YYYY>/<MM>/…`) or kept flat.
    /// `None` = no directive declared, so the store's built-in default for the
    /// type applies ([`crate::store::Store::type_shards`]); `Some(true)` forces
    /// date-sharding (e.g. a custom event type the toolkit has no built-in for);
    /// `Some(false)` forces flat. This is the v0.2 generic-model way to declare
    /// sharding — the toolkit ships no implicit per-type behavior beyond the
    /// example-type defaults.
    pub shard: Option<bool>,
}

/// One field declaration inside a [`Schema`]: `- <name> (<modifiers>)`.
///
/// Modifiers are comma-separated inside the parens; this captures the
/// recognized ones as typed fields and stashes anything unrecognized in
/// [`unknown_modifiers`](FieldSpec::unknown_modifiers) (surfaced as `Info`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FieldSpec {
    /// The field name.
    pub name: String,
    /// `required` modifier present.
    pub required: bool,
    /// The shape modifier (`string`/`int`/`bool`/`date`/`email`/`currency`/
    /// `url`), if any.
    pub shape: Option<Shape>,
    /// `link to <prefix>/` — the store-relative prefix a wiki-link target must
    /// start with. The trailing slash is required in the source syntax.
    pub link_prefix: Option<PathBuf>,
    /// `default <value>` — the value written when the field is absent.
    pub default: Option<Value>,
    /// `enum: <v1>, <v2>, ...` — the allowed values (must be the last modifier
    /// on the line because of its own commas).
    pub enum_values: Option<Vec<String>>,
    /// Any modifiers not in the recognized vocabulary, preserved verbatim;
    /// validate surfaces these as `Info`, never errors.
    pub unknown_modifiers: Vec<String>,
}

/// A recognized shape modifier for a schema field. Validate enforces the
/// corresponding value shape (`SCHEMA_SHAPE_MISMATCH` on violation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Any scalar string.
    String,
    /// Integer.
    Int,
    /// Boolean.
    Bool,
    /// RFC3339 / ISO-8601 date.
    Date,
    /// `<local>@<domain>` email address.
    Email,
    /// A currency amount.
    Currency,
    /// A URL.
    Url,
}

/// The result of splitting a raw file into its frontmatter block and body.
///
/// `body` is the verbatim remainder after the closing `---` fence — the writer
/// preserves it byte-for-byte so operator edits are never reflowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFile {
    /// The raw frontmatter YAML (between the fences, exclusive of them).
    pub frontmatter_yaml: String,
    /// The verbatim body (everything after the closing `---`).
    pub body: String,
}

/// Split a file's full text into its frontmatter block and body. The
/// frontmatter block must be the very first thing in the file, delimited by
/// `---` on its own line at start and end. Returns
/// [`ParseError::MissingFrontmatter`] if absent.
pub fn split_frontmatter(text: &str, file: &Path) -> Result<ParsedFile, ParseError> {
    // The opening fence must be the very first line: `---` (optionally with a
    // trailing CR), no leading whitespace, nothing before it.
    let mut lines = text.split_inclusive('\n');
    let first = lines.next().unwrap_or("");
    if first.trim_end_matches(['\r', '\n']) != "---" {
        return Err(ParseError::MissingFrontmatter {
            file: file.to_path_buf(),
        });
    }

    // Scan for the closing fence line. Track byte offsets so we can slice the
    // YAML (between fences, exclusive) and the body (verbatim, after the
    // closing fence's line terminator).
    let opening_len = first.len();
    let mut offset = opening_len;
    for line in lines {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            let yaml = &text[opening_len..offset];
            let body_start = offset + line.len();
            let body = &text[body_start..];
            return Ok(ParsedFile {
                frontmatter_yaml: yaml.to_string(),
                body: body.to_string(),
            });
        }
        offset += line.len();
    }

    // Opening fence present but no closing fence: malformed frontmatter block.
    Err(ParseError::MissingFrontmatter {
        file: file.to_path_buf(),
    })
}

/// Read a file from disk and parse it into typed [`Frontmatter`] plus the
/// verbatim body string.
pub fn read_file(path: &Path) -> Result<(Frontmatter, String), ParseError> {
    let text = std::fs::read_to_string(path)?;
    let parsed = split_frontmatter(&text, path)?;
    let fm = Frontmatter::parse(&parsed.frontmatter_yaml, path)?;
    Ok((fm, parsed.body))
}

/// Atomically write a markdown file from frontmatter + body: emit the
/// frontmatter in canonical key order, then the body verbatim, via a
/// temp-file-rename so a reader never sees a half-written file. Preserves the
/// operator-edited body exactly as given.
pub fn write_file(path: &Path, frontmatter: &Frontmatter, body: &str) -> Result<(), ParseError> {
    let yaml = frontmatter.to_yaml();
    // `to_yaml` already terminates each block with a newline. Compose the file
    // as: opening fence, frontmatter YAML, closing fence, then body verbatim.
    let mut contents = String::with_capacity(yaml.len() + body.len() + 8);
    contents.push_str("---\n");
    contents.push_str(&yaml);
    contents.push_str("---\n");
    contents.push_str(body);

    // One durable, atomic write for all primary data (see `crate::fsx`):
    // temp-file + fsync + rename + parent-fsync. Content records are primary
    // data, so they get the durable path (unlike the rebuildable index).
    crate::fsx::write_atomic(path, contents.as_bytes())?;
    Ok(())
}

/// Extract every wiki-link from a body (and inline frontmatter), returning the
/// structured [`WikiLink`] stream with short-form / `.md`-extension flags and
/// `(file, line, col)` locations set.
pub fn extract_wiki_links(body: &str, file: &Path) -> Vec<WikiLink> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        // [[target]] or [[target|display]]; target/display exclude brackets and
        // (for target) the `|` separator so nested forms don't over-match.
        regex::Regex::new(r"\[\[([^\[\]|]+?)(?:\|([^\[\]]*))?\]\]").expect("valid wiki-link regex")
    });

    let mut out = Vec::new();
    for (line_idx, line) in body.lines().enumerate() {
        for caps in re.captures_iter(line) {
            let whole = caps.get(0).expect("group 0 always present");
            let target = caps.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
            let display = caps.get(2).map(|m| m.as_str().to_string());
            out.push(WikiLink {
                is_full_path: target_is_full_path(&target),
                has_md_extension: target_has_md_extension(&target),
                target,
                display,
                location: (
                    file.to_path_buf(),
                    (line_idx as u32) + 1,
                    char_column(line, whole.start()),
                ),
            });
        }
    }
    out
}

/// Extract every standard markdown link `[text](url)` from a body into a
/// separate stream, kept distinct from wiki-links.
pub fn extract_markdown_links(body: &str, file: &Path) -> Vec<MarkdownLink> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        // [text](url). `text` excludes brackets so a wiki-link `[[x]]` (which
        // has `]]`, not `](`) never matches; `url` excludes `)` and whitespace.
        regex::Regex::new(r"\[([^\[\]]*)\]\(([^)\s]*)\)").expect("valid markdown-link regex")
    });

    let mut out = Vec::new();
    for (line_idx, line) in body.lines().enumerate() {
        for caps in re.captures_iter(line) {
            let whole = caps.get(0).expect("group 0 always present");
            out.push(MarkdownLink {
                text: caps.get(1).map(|m| m.as_str()).unwrap_or("").to_string(),
                url: caps.get(2).map(|m| m.as_str()).unwrap_or("").to_string(),
                location: (
                    file.to_path_buf(),
                    (line_idx as u32) + 1,
                    char_column(line, whole.start()),
                ),
            });
        }
    }
    out
}

/// Detect the frontmatter wiki-link-list mis-encoding: a wiki-link *list*
/// written so YAML parses it as nested sequences instead of a clean list of
/// strings. Returns the offending keys so validate can emit
/// `WIKI_LINK_FLOW_FORM_LIST`.
///
/// The subtlety is that `[[x]]` is YAML for "a list containing `[x]`", so the
/// shapes nest:
///
/// - **Scalar inline** `company: [[records/x]]` → `Seq[ Seq[String] ]`
///   (double-nested). This is the spec's scalar wiki-link form — NOT flagged.
/// - **Flow list** `attendees: [[[a]], [[b]]]` → `Seq[ Seq[Seq[String]], … ]`
///   (triple-nested). The list mis-encoding — flagged.
/// - **Unquoted block list** (`- [[a]]` per line) → also triple-nested, so it
///   is flagged too; the canonical list form must quote each item
///   (`- "[[a]]"`), which parses to a clean `Seq[String, …]` and is NOT flagged.
///
/// So the discriminator is nesting depth: a *list* mis-encoding has at least one
/// item that is itself a sequence-of-sequences, whereas a scalar inline link's
/// single item is a sequence-of-scalars.
pub fn detect_flow_form_link_lists(frontmatter_yaml: &str) -> Vec<String> {
    let value: Value = match serde_norway::from_str(frontmatter_yaml) {
        Ok(v) => v,
        // Malformed YAML is FM_MALFORMED_YAML's job, not ours; report nothing.
        Err(_) => return Vec::new(),
    };
    let Value::Mapping(map) = value else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for (k, v) in &map {
        if let Value::Sequence(items) = v {
            // Triple-nesting: some outer item is a sequence that itself holds a
            // sequence. Scalar inline `[[x]]` is only double-nested, so it
            // never matches.
            let is_link_list = items.iter().any(|item| match item {
                Value::Sequence(inner) => inner.iter().any(|x| matches!(x, Value::Sequence(_))),
                _ => false,
            });
            if is_link_list {
                if let Some(key) = k.as_str() {
                    out.push(key.to_string());
                }
            }
        }
    }
    out
}

/// Extract the `##`/`###` sections of a markdown body into a flat list with
/// body slices.
pub fn extract_sections(body: &str) -> Vec<Section> {
    // Keep each line's start so we can slice the body verbatim (exact newlines).
    let lines: Vec<&str> = body.split_inclusive('\n').collect();

    // First pass: classify heading levels (0 = not a heading), honoring fenced
    // code blocks so a `## x` inside a ``` fence is not treated as a heading.
    let mut levels: Vec<u8> = Vec::with_capacity(lines.len());
    let mut fence: Option<(u8, usize)> = None;
    for line in &lines {
        let content = line.trim_end_matches(['\n', '\r']);
        if let Some(f) = fence {
            if is_closing_fence(content, f) {
                fence = None;
            }
            levels.push(0);
            continue;
        }
        if let Some(opened) = opening_fence(content) {
            fence = Some(opened);
            levels.push(0);
            continue;
        }
        levels.push(heading_level(content));
    }

    // Second pass: emit `##`+ headings; each section body runs from its heading
    // line to the next heading at an equal-or-shallower level (exclusive).
    let mut sections = Vec::new();
    for (i, &lvl) in levels.iter().enumerate() {
        if lvl < 2 {
            continue;
        }
        let heading_line = lines[i].trim_end_matches(['\n', '\r']);
        let heading = heading_text(heading_line, lvl);

        let mut end = lines.len();
        for (j, &other) in levels.iter().enumerate().skip(i + 1) {
            if other != 0 && other <= lvl {
                end = j;
                break;
            }
        }

        sections.push(Section {
            heading,
            level: lvl,
            line: (i + 1) as u32,
            body: lines[i..end].concat(),
        });
    }
    sections
}

/// Parse a store's `DB.md` file into a [`Config`]: the `## Agent instructions`
/// prose, `## Policies` (`### Frozen pages` + `### Ignored types`), and
/// `## Schemas` (`### <type>` field-bullet blocks). Unrecognized sections are
/// ignored; absent sections leave their [`Config`] fields at default.
pub fn parse_db_md(text: &str, file: &Path) -> Result<Config, ParseError> {
    // The structured sections live in the body (after frontmatter). DB.md must
    // still start with a valid `---` block (`type: db-md`); if it's missing we
    // surface MissingFrontmatter like any other file.
    let parsed = split_frontmatter(text, file)?;
    let _frontmatter = Frontmatter::parse(&parsed.frontmatter_yaml, file)?;
    let sections = extract_sections(&parsed.body);

    let mut config = Config::default();
    // Track which H2 region each H3 belongs to as we walk the flat list.
    let mut current_h2: Option<String> = None;

    for section in &sections {
        match section.level {
            2 => {
                let name = section.heading.trim().to_ascii_lowercase();
                current_h2 = Some(name.clone());
                if name == "agent instructions" {
                    let prose = section_prose(&section.body);
                    if !prose.is_empty() {
                        config.agent_instructions = Some(prose);
                    }
                }
            }
            3 => {
                let h2 = current_h2.as_deref().unwrap_or("");
                let h3 = section.heading.trim().to_ascii_lowercase();
                match (h2, h3.as_str()) {
                    ("policies", "frozen pages") => {
                        config.frozen_pages = bullet_lines(&section.body)
                            .into_iter()
                            .map(|b| PathBuf::from(extract_path_bullet(&b)))
                            .collect();
                    }
                    ("policies", "ignored types") => {
                        config.ignored_types = bullet_lines(&section.body)
                            .into_iter()
                            .flat_map(|b| extract_type_list_bullet(&b))
                            .collect();
                    }
                    ("schemas", _) => {
                        // The H3 heading text (as written) is the type name.
                        let type_name = section.heading.trim().to_string();
                        let mut schema = Schema::default();
                        for b in bullet_lines(&section.body) {
                            match parse_schema_bullet(&b) {
                                SchemaBullet::Field(f) => schema.fields.push(f),
                                SchemaBullet::Unique(k) if !k.is_empty() => {
                                    schema.unique_keys.push(k)
                                }
                                SchemaBullet::SummaryTemplate(t) if !t.is_empty() => {
                                    schema.summary_template = Some(t)
                                }
                                SchemaBullet::Shard(Some(b)) => schema.shard = Some(b),
                                // Empty `unique:`/`summary_template:`, or a `shard:`
                                // with an unrecognized value — ignored.
                                SchemaBullet::Unique(_)
                                | SchemaBullet::SummaryTemplate(_)
                                | SchemaBullet::Shard(None) => {}
                            }
                        }
                        config.schemas.insert(type_name, schema);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    Ok(config)
}

/// One parsed bullet inside a `### <type>` schema block: an ordinary field, or a
/// reserved directive (`unique:` / `summary_template:` / `shard:`). The names
/// `unique`, `summary_template`, and `shard` are reserved and cannot be used as
/// field names.
#[derive(Debug)]
enum SchemaBullet {
    /// An ordinary `- <name> (<modifiers>)` field.
    Field(FieldSpec),
    /// `- unique: <field>[, <field> …]` — a (possibly compound) uniqueness key.
    Unique(Vec<String>),
    /// `- summary_template: <template>` — the default-`summary` pattern.
    SummaryTemplate(String),
    /// `- shard: by-date | flat` — date-shard records of this type, or keep them
    /// flat. `None` = an unrecognized value, ignored like an unknown modifier.
    Shard(Option<bool>),
}

/// Classify one `## Schemas` bullet as a directive or a field. The directive
/// forms are `- unique: a, b, …` and `- summary_template: …`; the keyword check
/// guards against false positives — a field like `- status (enum: a, b)` has a
/// `(` before any `:`, so its head isn't a bare reserved keyword and it parses
/// as a [`FieldSpec`].
fn parse_schema_bullet(bullet_line: &str) -> SchemaBullet {
    let line = bullet_line.trim();
    let line = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("+ "))
        .or_else(|| line.strip_prefix('-'))
        .unwrap_or(line)
        .trim();

    if let Some((head, rest)) = line.split_once(':') {
        match head.trim().to_ascii_lowercase().as_str() {
            "unique" => {
                let fields = rest
                    .split(',')
                    .map(|f| f.trim().to_string())
                    .filter(|f| !f.is_empty())
                    .collect();
                return SchemaBullet::Unique(fields);
            }
            "summary_template" => {
                return SchemaBullet::SummaryTemplate(rest.trim().to_string());
            }
            "shard" => {
                // `by-date` (synonyms: date/sharded/true) enables date-sharding;
                // `flat` (none/false) forces flat; anything else is ignored.
                let v = match rest.trim().to_ascii_lowercase().as_str() {
                    "by-date" | "date" | "sharded" | "true" => Some(true),
                    "flat" | "none" | "false" => Some(false),
                    _ => None,
                };
                return SchemaBullet::Shard(v);
            }
            _ => {}
        }
    }

    SchemaBullet::Field(parse_field_spec(bullet_line))
}

/// Parse a single `## Schemas` field-bullet line — `- <name> (<modifiers>)` —
/// into a [`FieldSpec`], capturing recognized modifiers and stashing the rest
/// in [`FieldSpec::unknown_modifiers`].
pub fn parse_field_spec(bullet_line: &str) -> FieldSpec {
    // Strip the leading bullet marker (`- ` / `* ` / `+ `) and surrounding ws.
    let line = bullet_line.trim();
    let line = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("+ "))
        .or_else(|| line.strip_prefix('-'))
        .unwrap_or(line)
        .trim();

    // Split `<name> (<modifiers>)`. A bullet without parens is a free-form
    // optional field of any shape — name only, no modifiers.
    let (name, modifiers) = match line.find('(') {
        Some(open) => {
            let name = line[..open].trim().to_string();
            let after = &line[open + 1..];
            let mods = match after.rfind(')') {
                Some(close) => &after[..close],
                None => after, // tolerate a missing close paren
            };
            (name, mods.trim())
        }
        None => (line.to_string(), ""),
    };

    let mut spec = FieldSpec {
        name,
        ..FieldSpec::default()
    };

    if modifiers.is_empty() {
        return spec;
    }

    // Modifiers are comma-separated. `enum:` is special: because its own value
    // list contains commas, it must be last and swallows the remainder.
    let raw: Vec<&str> = modifiers.split(',').collect();
    let mut i = 0;
    while i < raw.len() {
        let token = raw[i].trim();
        if token.is_empty() {
            i += 1;
            continue;
        }
        let lower = token.to_ascii_lowercase();

        if lower == "required" {
            spec.required = true;
        } else if let Some(shape) = shape_from_str(&lower) {
            spec.shape = Some(shape);
        } else if let Some(rest) = lower.strip_prefix("link to ") {
            // The trailing slash is required in the source; store the prefix
            // without it so `Path::starts_with` comparisons are clean.
            let prefix = token["link to ".len()..].trim().trim_end_matches('/');
            let _ = rest; // lowercase form only used for the keyword match
            spec.link_prefix = Some(PathBuf::from(prefix));
        } else if let Some(_rest) = lower.strip_prefix("default ") {
            // Value is everything after the keyword on this comma-token,
            // preserving original case.
            let value = token["default ".len()..].trim().to_string();
            spec.default = Some(Value::String(value));
        } else if lower == "enum" {
            // Bare `enum` keyword (`enum, open, closed`): the values are the
            // REMAINING tokens — the keyword itself must not leak in as a value.
            let values: Vec<String> = raw[i + 1..]
                .iter()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .collect();
            spec.enum_values = Some(values);
            break; // enum consumed the rest of the line
        } else if lower.starts_with("enum:") {
            // `enum: open, closed` form: rejoin this token and the rest, then
            // drop everything up to and including the `:`.
            let mut joined = raw[i..].join(",");
            if let Some(colon) = joined.find(':') {
                joined = joined[colon + 1..].to_string();
            }
            let values: Vec<String> = joined
                .split(',')
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .collect();
            spec.enum_values = Some(values);
            break; // enum consumed the rest of the line
        } else {
            // Unrecognized modifier — captured verbatim, surfaced as Info.
            spec.unknown_modifiers.push(token.to_string());
        }
        i += 1;
    }

    spec
}

// ── Private helpers ─────────────────────────────────────────────────────────

/// Parse a frontmatter timestamp value into a `DateTime<FixedOffset>`. A `null`
/// is treated as absent; anything else must be an RFC3339 string.
fn parse_timestamp(
    value: &Value,
    key: &str,
    file: &Path,
) -> Result<Option<DateTime<FixedOffset>>, ParseError> {
    match value {
        Value::Null => Ok(None),
        Value::String(s) => parse_rfc3339(s, key, file).map(Some),
        other => Err(ParseError::BadTimestamp {
            file: file.to_path_buf(),
            key: key.to_string(),
            value: format!("{other:?}"),
        }),
    }
}

/// Parse an RFC3339 timestamp string, mapping failure to [`ParseError::BadTimestamp`].
fn parse_rfc3339(s: &str, key: &str, file: &Path) -> Result<DateTime<FixedOffset>, ParseError> {
    DateTime::parse_from_rfc3339(s.trim()).map_err(|_| ParseError::BadTimestamp {
        file: file.to_path_buf(),
        key: key.to_string(),
        value: s.to_string(),
    })
}

/// Read a `tags` value into a flat `Vec<String>`. Accepts a sequence of scalars
/// (the canonical form) or a single scalar (coerced to a one-element list).
fn parse_tags(value: &Value) -> Vec<String> {
    match value {
        Value::Sequence(items) => items
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                Value::Bool(b) => Some(b.to_string()),
                _ => None,
            })
            .collect(),
        Value::String(s) => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// Parse a single `[[target|display]]` string into a [`WikiLink`] with no
/// location, or `None` if the string is not a bare wiki-link. Used for
/// frontmatter-valued links where there is no body position to report.
fn parse_wiki_link_str(s: &str) -> Option<WikiLink> {
    let s = s.trim();
    let inner = s.strip_prefix("[[")?.strip_suffix("]]")?;
    // Reject anything with further brackets (e.g. the nested flow-form item),
    // which is not a clean single wiki-link.
    if inner.contains('[') || inner.contains(']') {
        return None;
    }
    let (target, display) = match inner.split_once('|') {
        Some((t, d)) => (t.to_string(), Some(d.to_string())),
        None => (inner.to_string(), None),
    };
    Some(WikiLink {
        is_full_path: target_is_full_path(&target),
        has_md_extension: target_has_md_extension(&target),
        target,
        display,
        location: (PathBuf::new(), 0, 0),
    })
}

/// Extract every wiki-link from a single frontmatter field value, accepting the
/// two canonical forms the spec defines (SPEC § Linking):
///
/// - a **scalar** wiki-link field, in either the quoted (`f: "[[x]]"`) or the
///   canonical unquoted inline (`f: [[x]]`) form, and
/// - a **list** field whose items are quoted wiki-link strings
///   (`- "[[x]]"`).
///
/// YAML eats the brackets of an unquoted `[[x]]`, leaving a flow-list-in-a-list,
/// so the parsed [`Value`] shapes are not what one would naively expect:
///
/// | source                         | parsed `Value`                     | here |
/// |--------------------------------|------------------------------------|------|
/// | `f: "[[x]]"`       (quoted)    | `String("[[x]]")`                  | link |
/// | `f: [[x]]`         (unquoted)  | `Seq[ Seq[String("x")] ]`          | link |
/// | `f:`\n`  - "[[x]]"`(quoted)    | `Seq[ String("[[x]]"), … ]`        | link |
/// | `f:`\n`  - [[x]]`  (unquoted)  | `Seq[ Seq[Seq[String("x")]], … ]`  | —    |
///
/// The last row — an *unquoted list* — parses identically to the flow-form list
/// `f: [[a], [b]]` and is a mis-encoding the canonical writer never emits;
/// `dbmd validate` reports it as `WIKI_LINK_FLOW_FORM_LIST` (see
/// [`detect_flow_form_link_lists`]). It is deliberately NOT surfaced here, so an
/// edge enumerator only ever sees the valid canonical forms.
///
/// The unquoted scalar (`Seq[Seq[String]]`, one element) is told apart from a
/// plain one-item flow list (`f: [x]` → `Seq[String]`, one fewer nesting level)
/// by [`unquoted_inline_link`] requiring its argument to be a `Sequence`.
fn links_in_field_value(value: &Value) -> Vec<WikiLink> {
    // Quoted scalar: `field: "[[x]]"`.
    if let Value::String(s) = value {
        return parse_wiki_link_str(s).into_iter().collect();
    }
    let Value::Sequence(items) = value else {
        return Vec::new();
    };
    // Unquoted scalar inline form `field: [[x]]` → `Seq[ Seq[String(x)] ]`.
    // (A quoted single-item list `["[[x]]"]` is `Seq[String]`, so its lone item
    // is a `String`, not a `Sequence`, and falls through to the list path below.)
    if items.len() == 1 {
        if let Some(link) = unquoted_inline_link(&items[0]) {
            return vec![link];
        }
    }
    // Otherwise a list of quoted wiki-link strings; non-string items (the
    // unquoted-list mis-encoding) are left for validate to flag.
    items
        .iter()
        .filter_map(|item| parse_wiki_link_str(item.as_str()?))
        .collect()
}

/// Canonicalize one `extra` frontmatter value for emission by [`Frontmatter::to_yaml`].
///
/// The read path ([`Frontmatter::parse`]) stores every unknown key's raw parsed
/// [`Value`] verbatim, so a SPEC-canonical *unquoted* inline scalar wiki-link
/// (`company: [[records/companies/northstar]]`) lands in `extra` as the nested
/// shape YAML produces for it — `Seq[ Seq[String("records/companies/northstar")] ]`.
/// Re-emitting that verbatim yields the block sequence
///
/// ```text
/// company:
/// - - records/companies/northstar
/// ```
///
/// which has lost the `[[ ]]` brackets entirely: the link is destroyed, and every
/// reader (validate, graph, backlinks) stops seeing the edge. This normalizes such
/// a value back into the canonical emitted form before it is written:
///
/// - a **scalar** wiki-link (quoted `String("[[x]]")` or unquoted `Seq[Seq[String]]`,
///   one element) → a quoted scalar `Value::String("[[x]]")`, which serde_norway emits
///   inline as `'[[x]]'` — the form the finding confirms survives a round-trip and
///   that [`links_in_field_value`] reads back as the same scalar link;
/// - a **list** of wiki-links (in any spelling [`links_in_field_value`] accepts) →
///   a block `Value::Sequence` of quoted-link strings (`- "[[x]]"`), matching the
///   `set` write-in path and the canonical list form;
/// - everything else → returned verbatim (the common no-op for non-link values).
///
/// `|display` is preserved in both link branches. This is the single point that
/// keeps all three curator-loop writers (`format`, `fm set`, `link`) from
/// corrupting a pre-existing canonical link, since they all funnel through
/// `to_yaml`.
fn canonicalize_extra_value(value: &Value) -> Value {
    match value {
        // Scalar wiki-link, quoted form: `field: "[[x]]"` → `String("[[x]]")`.
        // Re-emit as a quoted scalar so it stays a string (never the brackets-as-
        // YAML nested sequence). Non-link strings are returned untouched.
        Value::String(s) => match parse_wiki_link_str(s) {
            Some(link) => Value::String(wiki_link_literal(&link)),
            None => value.clone(),
        },
        Value::Sequence(items) => {
            // Scalar wiki-link, unquoted inline form: `field: [[x]]` parses to a
            // one-element `Seq[ Seq[String(x)] ]`. Collapse back to the quoted
            // scalar string so the link is preserved rather than block-emitted.
            if items.len() == 1 {
                if let Some(link) = unquoted_inline_link(&items[0]) {
                    return Value::String(wiki_link_literal(&link));
                }
            }
            // List of wiki-links: re-emit as a block sequence of quoted-link
            // strings, the canonical list form `to_yaml` renders block-style and
            // `links_in_field_value` accepts. Only canonicalize when *every* item
            // is a clean single wiki-link; a list with any non-link item is left
            // verbatim so unrelated sequences (and the unquoted-list mis-encoding
            // validate flags) are untouched.
            let mut links = Vec::with_capacity(items.len());
            for item in items {
                match link_from_flow_list_item(item) {
                    Some(link) => links.push(link),
                    None => return value.clone(),
                }
            }
            if links.is_empty() {
                return value.clone();
            }
            Value::Sequence(
                links
                    .iter()
                    .map(|l| Value::String(wiki_link_literal(l)))
                    .collect(),
            )
        }
        // Mappings, scalars other than strings, nulls: nothing to canonicalize.
        _ => value.clone(),
    }
}

/// Render a [`WikiLink`] back to its `[[target]]` / `[[target|display]]` literal,
/// the inner form the canonical writer emits and `links_in_field_value` accepts.
fn wiki_link_literal(link: &WikiLink) -> String {
    match &link.display {
        Some(d) => format!("[[{}|{}]]", link.target, d),
        None => format!("[[{}]]", link.target),
    }
}

/// Recognize the inner token of an unquoted scalar `[[x]]`: after YAML strips the
/// outer brackets, the inner `[x]` is a single-element sequence `Seq[String(x)]`.
/// Reconstructs `[[x]]` (preserving any `|display`) and parses it, or returns
/// `None` when `v` is not that shape. Requiring a `Sequence` here is what keeps a
/// plain one-item flow list (`field: [x]` → `Seq[String]`, not `Seq[Seq[String]]`)
/// from being mistaken for a wiki-link.
fn unquoted_inline_link(v: &Value) -> Option<WikiLink> {
    let Value::Sequence(items) = v else {
        return None;
    };
    if items.len() != 1 {
        return None;
    }
    let s = items[0].as_str()?;
    // A clean unquoted wiki-link has no further brackets inside it.
    if s.contains('[') || s.contains(']') {
        return None;
    }
    parse_wiki_link_str(&format!("[[{s}]]"))
}

/// Decide whether a `dbmd fm set` / `--fm` value string is a **list of
/// wiki-links** that should be stored as a YAML block sequence, returning the
/// canonical `Value::Sequence` of quoted-link strings when so.
///
/// The value path of every write surface stringifies its argument; without this
/// a required list-of-links field (`meeting.attendees`) was unwritable in valid
/// form — passing `[[[a]], [[b]]]` stored a single scalar string that mis-parses
/// and trips `WIKI_LINK_FLOW_FORM_LIST` / `WIKI_LINK_BROKEN`. This recognizes the
/// two list spellings an agent naturally types and normalizes both to the block
/// form the canonical writer emits and `dbmd validate` accepts:
///
/// - flow list of quoted links — `["[[a]]", "[[b]]"]`
/// - flow list of unquoted links — `[[[a]], [[b]]]` (YAML: `Seq[Seq[String], …]`)
///
/// Returns `None` (⇒ caller stores a verbatim scalar string) for everything that
/// is not unambiguously a list of clean wiki-links — plain text, a single inline
/// `[[x]]` (YAML reads it as a one-item `Seq[Seq[String]]`, kept scalar so it
/// renders inline), an empty list, or a list with any non-link item. A single
/// link must stay scalar; only genuine multi-item-or-explicit lists become
/// sequences, matching `links_in_field_value`'s acceptance rule so writer and
/// validator never disagree.
fn parse_link_list_value(value: &str) -> Option<Value> {
    let trimmed = value.trim();
    // Only a YAML *flow sequence* literal is a list candidate; anything not
    // wrapped in `[ … ]` is a scalar (a bare `[[x]]` is wrapped, and handled by
    // the single-inline-link guard below).
    if !(trimmed.starts_with('[') && trimmed.ends_with(']')) {
        return None;
    }
    let Ok(Value::Sequence(items)) = serde_norway::from_str::<Value>(trimmed) else {
        return None;
    };
    // A single inline `[[x]]` parses to `Seq[ Seq[String(x)] ]` (one item, itself
    // a sequence) — that is the unquoted *scalar* form, not a list. Keep it scalar
    // so it round-trips to the inline `field: [[x]]` rather than a one-item block
    // list. `links_in_field_value` reads it back as a scalar link either way.
    if items.len() == 1 && unquoted_inline_link(&items[0]).is_some() {
        return None;
    }
    // Every item must resolve to exactly one clean wiki-link, in any of the flow
    // spellings an agent types (see [`link_from_flow_list_item`]).
    let mut links = Vec::with_capacity(items.len());
    for item in &items {
        links.push(link_from_flow_list_item(item)?);
    }
    if links.is_empty() {
        return None;
    }
    // Normalize to a block sequence of quoted-link strings — the form `to_yaml`
    // renders block-style and `links_in_field_value` accepts. `|display` is
    // preserved.
    let normalized = links
        .iter()
        .map(|l| Value::String(wiki_link_literal(l)))
        .collect();
    Some(Value::Sequence(normalized))
}

/// Recognize one clean wiki-link from a single **item** of a YAML flow sequence,
/// across the spellings an agent types for a list. After top-level flow parsing,
/// a list item arrives in one of:
///
/// - quoted — `"[[x]]"` ⇒ `String("[[x]]")`
/// - unquoted in a flow list — `[[x]]` inside `[…]` ⇒ `Seq[ Seq[String(x)] ]`
///   (one level deeper than a bare unquoted scalar, because the surrounding list
///   adds a wrapper); unwrap the single-element wrapper, then read the inline
///   `Seq[String(x)]` with [`unquoted_inline_link`].
///
/// Returns `None` for any item that is not exactly one clean wiki-link, so the
/// caller falls back to a scalar string and never fabricates a partial list.
fn link_from_flow_list_item(item: &Value) -> Option<WikiLink> {
    match item {
        Value::String(s) => parse_wiki_link_str(s),
        Value::Sequence(inner) => {
            // Unquoted list item `[[x]]` → `Seq[ Seq[String(x)] ]`: peel the lone
            // wrapper to expose the inline-link shape.
            if inner.len() == 1 {
                if let Some(link) = unquoted_inline_link(&inner[0]) {
                    return Some(link);
                }
            }
            // Defensive: also accept the inline-link shape directly.
            unquoted_inline_link(item)
        }
        _ => None,
    }
}

/// A target is a full store-relative path when its first path segment is one of
/// the three canonical layer dirs and at least one `/` separator follows. A
/// trailing `.md` does not affect this classification.
fn target_is_full_path(target: &str) -> bool {
    let target = target.trim();
    match target.split_once('/') {
        Some((head, _rest)) => LAYER_DIRS.contains(&head),
        None => false,
    }
}

/// True when the target carries a trailing `.md` extension (validate warns
/// `WIKI_LINK_HAS_EXTENSION`).
fn target_has_md_extension(target: &str) -> bool {
    target.trim().ends_with(".md")
}

/// 1-based character (Unicode scalar) column of `byte_offset` within `line`.
fn char_column(line: &str, byte_offset: usize) -> u32 {
    (line[..byte_offset].chars().count() as u32) + 1
}

/// Map a lowercase shape keyword to its [`Shape`].
fn shape_from_str(s: &str) -> Option<Shape> {
    match s {
        "string" => Some(Shape::String),
        "int" => Some(Shape::Int),
        "bool" => Some(Shape::Bool),
        "date" => Some(Shape::Date),
        "email" => Some(Shape::Email),
        "currency" => Some(Shape::Currency),
        "url" => Some(Shape::Url),
        _ => None,
    }
}

/// The ATX heading level of a line (number of leading `#`), or 0 if not a
/// heading. Up to three leading spaces (CommonMark), requires a space/tab (or
/// end-of-line) after the `#` run, caps the run at six.
fn heading_level(line: &str) -> u8 {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return 0;
    }
    let rest = &line[indent..];
    let hashes = rest.len() - rest.trim_start_matches('#').len();
    if hashes == 0 || hashes > 6 {
        return 0;
    }
    let after = &rest[hashes..];
    if after.is_empty() || after.starts_with(' ') || after.starts_with('\t') {
        hashes as u8
    } else {
        0
    }
}

/// The heading text after the `#` run, trimmed, with any trailing ATX closing
/// `#` sequence removed (`## Title ##` → `Title`).
fn heading_text(line: &str, level: u8) -> String {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let after_hashes = &line[indent + level as usize..];
    let trimmed = after_hashes.trim();
    let no_trailing = trimmed.trim_end_matches('#');
    if no_trailing.len() == trimmed.len() {
        trimmed.to_string()
    } else {
        no_trailing.trim_end().to_string()
    }
}

/// If `line` opens a fenced code block, return `(fence byte, run length)`.
fn opening_fence(line: &str) -> Option<(u8, usize)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let byte = rest.bytes().next()?;
    if byte != b'`' && byte != b'~' {
        return None;
    }
    let run = rest.len() - rest.trim_start_matches(byte as char).len();
    if run < 3 {
        return None;
    }
    // A backtick fence's info string may not itself contain a backtick.
    if byte == b'`' && rest[run..].contains('`') {
        return None;
    }
    Some((byte, run))
}

/// True if `line` closes the currently open fence: same char, run at least as
/// long, nothing but trailing whitespace after.
fn is_closing_fence(line: &str, fence: (u8, usize)) -> bool {
    let (byte, open_len) = fence;
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return false;
    }
    let rest = &line[indent..];
    let run = rest.len() - rest.trim_start_matches(byte as char).len();
    if run < open_len {
        return false;
    }
    rest[run..].trim().is_empty()
}

/// The prose body of a section: everything after the heading line, trimmed.
fn section_prose(section_body: &str) -> String {
    match section_body.split_once('\n') {
        Some((_heading, rest)) => rest.trim().to_string(),
        None => String::new(),
    }
}

/// The bullet lines (`-`/`*`/`+`) of a section body, excluding the heading
/// line, each returned with its leading whitespace trimmed.
fn bullet_lines(section_body: &str) -> Vec<String> {
    section_body
        .lines()
        .skip(1) // the heading line
        .map(str::trim)
        .filter(|l| l.starts_with("- ") || l.starts_with("* ") || l.starts_with("+ "))
        .map(|l| l.to_string())
        .collect()
}

/// Cut a bullet's content at the first ` — ` / ` -- ` comment separator,
/// returning only the meaningful prefix.
fn strip_bullet_comment(content: &str) -> &str {
    let mut cut = content.len();
    for sep in [" — ", " -- ", " – "] {
        if let Some(idx) = content.find(sep) {
            cut = cut.min(idx);
        }
    }
    content[..cut].trim()
}

/// Strip the leading bullet marker, returning the trimmed content after it.
fn bullet_content(bullet: &str) -> &str {
    let t = bullet.trim();
    t.strip_prefix("- ")
        .or_else(|| t.strip_prefix("* "))
        .or_else(|| t.strip_prefix("+ "))
        .unwrap_or(t)
        .trim()
}

/// Extract a store-relative path from a Frozen-pages bullet. The path may be
/// wrapped in backticks and followed by an em-dash comment.
fn extract_path_bullet(bullet: &str) -> String {
    let content = bullet_content(bullet);
    // Prefer a backtick-delimited span if present.
    if let Some(start) = content.find('`') {
        if let Some(end_rel) = content[start + 1..].find('`') {
            return content[start + 1..start + 1 + end_rel].trim().to_string();
        }
    }
    // Otherwise take the text up to a comment separator, stripping quotes.
    strip_bullet_comment(content)
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_string()
}

/// Extract a comma-separated type list from an Ignored-types bullet, stripping
/// backticks/quotes and any trailing em-dash comment.
fn extract_type_list_bullet(bullet: &str) -> Vec<String> {
    let content = strip_bullet_comment(bullet_content(bullet));
    content
        .split(',')
        .map(|t| {
            t.trim()
                .trim_matches('`')
                .trim_matches('"')
                .trim_matches('\'')
                .trim()
                .to_string()
        })
        .filter(|t| !t.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::tempdir;

    // ── Config::frozen_match (the single write-surface policy matcher) ───────

    #[test]
    fn frozen_match_is_md_insensitive_both_directions() {
        // A policy entry stored WITHOUT `.md` (the natural extensionless
        // spelling `parse_db_md` keeps verbatim) must still match a `.md`
        // write target — the regression every write surface had.
        let cfg = Config {
            frozen_pages: vec![PathBuf::from("records/decisions/q1")],
            ..Config::default()
        };
        assert_eq!(
            cfg.frozen_match(Path::new("records/decisions/q1.md")),
            Some(PathBuf::from("records/decisions/q1")),
            "extensionless policy entry must freeze the .md file"
        );
        assert!(cfg.is_frozen(Path::new("records/decisions/q1.md")));

        // The symmetric case: a policy entry WITH `.md` matches a bare target.
        let cfg = Config {
            frozen_pages: vec![PathBuf::from("records/decisions/q1.md")],
            ..Config::default()
        };
        assert_eq!(
            cfg.frozen_match(Path::new("records/decisions/q1")),
            Some(PathBuf::from("records/decisions/q1.md")),
        );
        // And the same-spelling cases still match.
        assert!(cfg.is_frozen(Path::new("records/decisions/q1.md")));
    }

    #[test]
    fn frozen_match_drops_leading_dot_slash() {
        let cfg = Config {
            frozen_pages: vec![PathBuf::from("records/decisions/q1.md")],
            ..Config::default()
        };
        assert!(cfg.is_frozen(Path::new("./records/decisions/q1.md")));
        assert!(cfg.is_frozen(Path::new("./records/decisions/q1")));
    }

    #[test]
    fn frozen_match_returns_none_for_unlisted_and_prefix_paths() {
        let cfg = Config {
            frozen_pages: vec![PathBuf::from("records/decisions/q1")],
            ..Config::default()
        };
        assert!(cfg
            .frozen_match(Path::new("records/decisions/q2.md"))
            .is_none());
        // A prefix is not a match: `q1` must not freeze `q1-draft`.
        assert!(cfg
            .frozen_match(Path::new("records/decisions/q1-draft.md"))
            .is_none());
        assert!(!cfg.is_frozen(Path::new("records/decisions/q11.md")));
    }

    // ── split_frontmatter ───────────────────────────────────────────────────

    #[test]
    fn split_frontmatter_separates_yaml_and_verbatim_body() {
        let text = "---\ntype: contact\nsummary: x\n---\n# Heading\n\nBody line.\n";
        let p = split_frontmatter(text, Path::new("f.md")).unwrap();
        assert_eq!(p.frontmatter_yaml, "type: contact\nsummary: x\n");
        // Body is everything after the closing fence's newline, byte-for-byte.
        assert_eq!(p.body, "# Heading\n\nBody line.\n");
    }

    #[test]
    fn split_frontmatter_preserves_body_without_trailing_newline() {
        let text = "---\ntype: x\n---\nno trailing newline";
        let p = split_frontmatter(text, Path::new("f.md")).unwrap();
        assert_eq!(p.body, "no trailing newline");
    }

    #[test]
    fn split_frontmatter_empty_body_when_nothing_after_fence() {
        let text = "---\ntype: x\n---\n";
        let p = split_frontmatter(text, Path::new("f.md")).unwrap();
        assert_eq!(p.body, "");
    }

    #[test]
    fn split_frontmatter_missing_opening_fence_errors() {
        let text = "# No frontmatter here\ntype: x\n";
        let err = split_frontmatter(text, Path::new("f.md")).unwrap_err();
        assert!(matches!(err, ParseError::MissingFrontmatter { .. }));
    }

    #[test]
    fn split_frontmatter_leading_content_before_fence_rejected() {
        // The opening fence must be the very first line; a blank line first is
        // not allowed.
        let text = "\n---\ntype: x\n---\nbody";
        let err = split_frontmatter(text, Path::new("f.md")).unwrap_err();
        assert!(matches!(err, ParseError::MissingFrontmatter { .. }));
    }

    #[test]
    fn split_frontmatter_unterminated_block_errors() {
        let text = "---\ntype: x\nsummary: y\n";
        let err = split_frontmatter(text, Path::new("f.md")).unwrap_err();
        assert!(matches!(err, ParseError::MissingFrontmatter { .. }));
    }

    // ── Frontmatter::parse ───────────────────────────────────────────────────

    #[test]
    fn parse_populates_typed_fields_and_routes_unknowns_to_extra() {
        let yaml = "type: contact\nid: sarah-chen\nsummary: Director of Ops\nstatus: active\ntags: [vip, renewal]\nemail: sarah@northstar.io\nrole: Director";
        let fm = Frontmatter::parse(yaml, Path::new("f.md")).unwrap();
        assert_eq!(fm.type_.as_deref(), Some("contact"));
        assert_eq!(fm.id.as_deref(), Some("sarah-chen"));
        assert_eq!(fm.summary.as_deref(), Some("Director of Ops"));
        assert_eq!(fm.status.as_deref(), Some("active"));
        assert_eq!(fm.tags, vec!["vip".to_string(), "renewal".to_string()]);
        // Type-specific fields are NOT promoted to typed slots.
        assert!(fm.type_.is_some() && !fm.extra.contains_key("type"));
        assert!(!fm.extra.contains_key("tags"));
        assert_eq!(
            fm.extra.get("email").and_then(|v| v.as_str()),
            Some("sarah@northstar.io")
        );
        assert_eq!(
            fm.extra.get("role").and_then(|v| v.as_str()),
            Some("Director")
        );
    }

    #[test]
    fn parse_reads_rfc3339_timestamps() {
        let yaml =
            "type: email\ncreated: 2026-05-27T08:00:00-07:00\nupdated: 2026-05-28T09:30:00-07:00";
        let fm = Frontmatter::parse(yaml, Path::new("f.md")).unwrap();
        let created = fm.created.expect("created parsed");
        // -07:00 offset is 7 * 3600 seconds west.
        assert_eq!(created.offset().utc_minus_local(), 7 * 3600);
        assert_eq!(created.to_rfc3339(), "2026-05-27T08:00:00-07:00");
        assert!(fm.updated.is_some());
    }

    #[test]
    fn parse_rejects_non_rfc3339_timestamp() {
        // A date-only value is not a full RFC3339 timestamp; created/updated
        // require the full form.
        let yaml = "type: email\ncreated: 2026-05-27";
        let err = Frontmatter::parse(yaml, Path::new("bad.md")).unwrap_err();
        match err {
            ParseError::BadTimestamp { key, value, .. } => {
                assert_eq!(key, "created");
                assert_eq!(value, "2026-05-27");
            }
            other => panic!("expected BadTimestamp, got {other:?}"),
        }
    }

    #[test]
    fn parse_malformed_yaml_errors() {
        // Unclosed flow mapping is invalid YAML.
        let yaml = "type: contact\n  bad: : :\n- nope";
        let err = Frontmatter::parse(yaml, Path::new("bad.md")).unwrap_err();
        assert!(matches!(err, ParseError::MalformedYaml { .. }));
    }

    #[test]
    fn frontmatter_with_yaml_tag_on_mapping_does_not_panic() {
        // Regression: a YAML tag on the top-level mapping made the old
        // `expect_err` path PANIC, because a tagged mapping deserializes to a
        // `Mapping` just fine. It must now be handled — accepted as the inner
        // mapping, never a panic.
        let fm = Frontmatter::parse("!mytag\ntype: contact\nsummary: hi\n", Path::new("x.md"))
            .expect("tagged-mapping frontmatter must parse, not panic");
        assert_eq!(fm.type_.as_deref(), Some("contact"));
        // A genuine scalar/sequence top level is still malformed (and still
        // doesn't panic).
        assert!(Frontmatter::parse("- a\n- b\n", Path::new("x.md")).is_err());
    }

    #[test]
    fn parse_empty_block_is_empty_frontmatter() {
        let fm = Frontmatter::parse("", Path::new("f.md")).unwrap();
        assert_eq!(fm, Frontmatter::default());
    }

    #[test]
    fn parse_scalar_top_level_is_malformed() {
        // A bare scalar at the top level is not a frontmatter mapping.
        let err = Frontmatter::parse("just a string", Path::new("f.md")).unwrap_err();
        assert!(matches!(err, ParseError::MalformedYaml { .. }));
    }

    // ── to_yaml canonical order ──────────────────────────────────────────────

    #[test]
    fn to_yaml_emits_canonical_key_order() {
        let mut fm = Frontmatter {
            type_: Some("contact".into()),
            id: Some("sarah-chen".into()),
            summary: Some("Director of Ops".into()),
            status: Some("active".into()),
            tags: vec!["vip".into()],
            created: Some(DateTime::parse_from_rfc3339("2026-05-27T08:00:00-07:00").unwrap()),
            updated: Some(DateTime::parse_from_rfc3339("2026-05-28T09:30:00-07:00").unwrap()),
            ..Default::default()
        };
        // Two type-specific fields, inserted in NON-alphabetical order to prove
        // the writer sorts them (BTreeMap) between the universal head and tail.
        fm.extra
            .insert("role".into(), Value::String("Director".into()));
        fm.extra.insert(
            "company".into(),
            Value::String("[[records/companies/northstar]]".into()),
        );

        let yaml = fm.to_yaml();
        let keys: Vec<&str> = yaml
            .lines()
            .filter(|l| !l.starts_with(['-', ' ']) && l.contains(':'))
            .map(|l| l.split(':').next().unwrap())
            .collect();
        assert_eq!(
            keys,
            vec![
                "type", "id", "created", "updated", "summary", // universal head
                "company", "role",   // type-specific, sorted
                "status", // universal tail
                "tags",
            ],
            "canonical order violated; got:\n{yaml}"
        );
        // Timestamps round-trip as RFC3339 strings (YAML may quote them).
        assert!(
            yaml.contains("2026-05-27T08:00:00-07:00"),
            "created timestamp missing; got:\n{yaml}"
        );
        // The value re-parses to the same instant regardless of quoting.
        let reparsed = Frontmatter::parse(&yaml, Path::new("rt.md")).unwrap();
        assert_eq!(reparsed.created, fm.created);
        assert_eq!(reparsed.updated, fm.updated);
    }

    #[test]
    fn to_yaml_omits_absent_optional_fields() {
        let fm = Frontmatter {
            type_: Some("note".into()),
            ..Default::default()
        };
        let yaml = fm.to_yaml();
        assert!(yaml.contains("type: note"));
        assert!(!yaml.contains("status"));
        assert!(!yaml.contains("tags"));
        assert!(!yaml.contains("summary"));
    }

    #[test]
    fn to_yaml_preserves_unquoted_scalar_wiki_link_round_trip() {
        // Regression (PRIMARY): the SPEC-canonical scalar wiki-link is the
        // *unquoted* inline `company: [[records/companies/northstar]]`
        // (SPEC § Linking, the worked `contact` example). YAML parses it to the
        // nested `Seq[Seq[String]]` shape and `parse` stores that verbatim in
        // `extra`. Before the fix, `to_yaml` re-emitted it block-style as
        //     company:
        //     - - records/companies/northstar
        // — the `[[ ]]` brackets GONE — so a no-op re-emit (`dbmd format`, and
        // any `fm set` / `link` write) silently destroyed the link.
        let yaml = "type: contact\ncompany: [[records/companies/northstar]]";
        let fm = Frontmatter::parse(yaml, Path::new("c.md")).unwrap();
        // Sanity: it really parsed as the nested sequence, not a string.
        assert!(fm.extra.get("company").and_then(|v| v.as_str()).is_none());

        let out = fm.to_yaml();
        // The link must survive as a quoted inline scalar — brackets intact, and
        // never the bracket-less block sequence `- - records/...`.
        assert!(
            out.contains("[[records/companies/northstar]]"),
            "canonical writer dropped the wiki-link brackets; got:\n{out}"
        );
        assert!(
            !out.contains("- - "),
            "canonical writer emitted a nested block sequence (link corrupted); got:\n{out}"
        );

        // And it round-trips: re-parsing the emitted YAML still surfaces exactly
        // one link with the right target (the edge graph/backlinks rely on).
        let reparsed = Frontmatter::parse(&out, Path::new("c.md")).unwrap();
        let fields = reparsed.link_fields();
        let links: Vec<(&str, &str, Option<&str>)> = fields
            .iter()
            .map(|(k, l)| (k.as_str(), l.target.as_str(), l.display.as_deref()))
            .collect();
        assert_eq!(
            links,
            vec![("company", "records/companies/northstar", None)]
        );

        // A second re-emit is a fixed point — no progressive corruption across
        // repeated curator-loop writes.
        assert_eq!(
            reparsed.to_yaml(),
            out,
            "to_yaml is not idempotent on links"
        );
    }

    #[test]
    fn to_yaml_preserves_unquoted_scalar_link_with_display() {
        // The `|display` segment must survive the unquoted-inline round-trip too.
        let yaml = "type: contact\ncompany: [[records/companies/northstar|Northstar]]";
        let fm = Frontmatter::parse(yaml, Path::new("c.md")).unwrap();
        let out = fm.to_yaml();
        assert!(
            out.contains("[[records/companies/northstar|Northstar]]"),
            "display segment lost on round-trip; got:\n{out}"
        );
        let reparsed = Frontmatter::parse(&out, Path::new("c.md")).unwrap();
        let f = reparsed.link_fields();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].1.target, "records/companies/northstar");
        assert_eq!(f[0].1.display.as_deref(), Some("Northstar"));
    }

    #[test]
    fn to_yaml_does_not_mangle_link_list_or_plain_nested_sequence() {
        // A genuine quoted block list of links round-trips as a clean string
        // list — never collapsed to a scalar — and a plain nested sequence that
        // is NOT a wiki-link is left exactly as written (no false conversion).
        let yaml = "type: meeting\nattendees:\n  - \"[[records/contacts/elena]]\"\n  - \"[[records/contacts/sarah]]\"\nmatrix:\n  - - 1\n    - 2";
        let fm = Frontmatter::parse(yaml, Path::new("m.md")).unwrap();
        let out = fm.to_yaml();

        // Both attendee links survive as quoted strings.
        assert!(out.contains("[[records/contacts/elena]]"), "got:\n{out}");
        assert!(out.contains("[[records/contacts/sarah]]"), "got:\n{out}");

        let reparsed = Frontmatter::parse(&out, Path::new("m.md")).unwrap();
        let fields = reparsed.link_fields();
        let attendees: Vec<&str> = fields
            .iter()
            .filter(|(k, _)| k == "attendees")
            .map(|(_, l)| l.target.as_str())
            .collect();
        assert_eq!(
            attendees,
            vec!["records/contacts/elena", "records/contacts/sarah"]
        );
        // The non-link nested sequence is preserved verbatim, not touched.
        assert_eq!(reparsed.extra.get("matrix"), fm.extra.get("matrix"));
    }

    // ── read_file / write_file round-trip ────────────────────────────────────

    #[test]
    fn write_then_read_roundtrips_and_preserves_body_verbatim() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sources/emails/x.md");
        let body = "# Subject\n\nHello,\n\nSee [[records/contacts/sarah-chen]].\n";
        let mut fm = Frontmatter {
            type_: Some("email".into()),
            summary: Some("renewal note".into()),
            created: Some(DateTime::parse_from_rfc3339("2026-05-27T08:00:00-07:00").unwrap()),
            ..Default::default()
        };
        fm.extra
            .insert("from".into(), Value::String("elena@northstar.io".into()));

        write_file(&path, &fm, body).unwrap();

        let (read_fm, read_body) = read_file(&path).unwrap();
        assert_eq!(read_body, body, "body must be preserved byte-for-byte");
        assert_eq!(read_fm.type_.as_deref(), Some("email"));
        assert_eq!(read_fm.summary.as_deref(), Some("renewal note"));
        assert_eq!(
            read_fm.extra.get("from").and_then(|v| v.as_str()),
            Some("elena@northstar.io")
        );
        // The on-disk file starts with a fence and ends with the verbatim body.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.starts_with("---\n"));
        assert!(raw.ends_with(body));
    }

    #[test]
    fn roundtrip_modify_summary_then_write_changes_only_summary() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("records/contacts/sarah.md");
        let body = "Long-form operator notes about Sarah.\n";
        let fm = Frontmatter {
            type_: Some("contact".into()),
            summary: Some("old summary".into()),
            ..Default::default()
        };
        write_file(&path, &fm, body).unwrap();

        // Read → modify summary → write back.
        let (mut fm2, body2) = read_file(&path).unwrap();
        fm2.summary = Some("new summary".into());
        write_file(&path, &fm2, &body2).unwrap();

        let (fm3, body3) = read_file(&path).unwrap();
        assert_eq!(fm3.summary.as_deref(), Some("new summary"));
        assert_eq!(fm3.type_.as_deref(), Some("contact"));
        assert_eq!(body3, body, "body unchanged across the round-trip");
    }

    #[test]
    fn roundtrip_preserves_handwritten_unquoted_scalar_wiki_link_on_disk() {
        // End-to-end analog of `dbmd format` on the verbatim SPEC worked example:
        // a hand-written file carrying the canonical UNQUOTED scalar link
        // `company: [[records/companies/northstar]]`, read from disk then written
        // back unchanged. Before the fix this no-op re-emit rewrote the on-disk
        // value to the bracket-less block sequence `company:\n- - records/...`,
        // and every reader (validate/graph/backlinks) then lost the edge.
        let dir = tempdir().unwrap();
        let path = dir.path().join("records/contacts/sarah-chen.md");
        let file = "---\ntype: contact\nid: sarah-chen\nsummary: Director of Ops\ncompany: [[records/companies/northstar]]\n---\n# Sarah Chen\n\nNotes.\n";
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, file).unwrap();

        // Read → write back unchanged (the canonical no-op re-emit).
        let (fm, body) = read_file(&path).unwrap();
        write_file(&path, &fm, &body).unwrap();

        // On-disk bytes still carry the bracketed link, never `- - records/...`.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("[[records/companies/northstar]]"),
            "on-disk wiki-link brackets were destroyed; got:\n{raw}"
        );
        assert!(
            !raw.contains("- - "),
            "on-disk value became a nested block sequence; got:\n{raw}"
        );

        // And the edge is still readable after the round-trip.
        let (fm2, _) = read_file(&path).unwrap();
        let fields = fm2.link_fields();
        let links: Vec<(&str, &str)> = fields
            .iter()
            .map(|(k, l)| (k.as_str(), l.target.as_str()))
            .collect();
        assert_eq!(links, vec![("company", "records/companies/northstar")]);
    }

    #[test]
    fn write_file_does_not_leave_temp_files_behind() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("records/x.md");
        let fm = Frontmatter {
            type_: Some("note".into()),
            ..Default::default()
        };
        write_file(&path, &fm, "body\n").unwrap();
        // The directory should contain only the target file, no `.x.md.tmp.*`.
        let entries: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["x.md".to_string()]);
    }

    // ── is_content_file ──────────────────────────────────────────────────────

    #[test]
    fn is_content_file_recognizes_layers_and_excludes_meta() {
        assert!(Frontmatter::is_content_file(Path::new(
            "sources/emails/2026-05-22.md"
        )));
        assert!(Frontmatter::is_content_file(Path::new(
            "records/contacts/sarah-chen.md"
        )));
        assert!(Frontmatter::is_content_file(Path::new(
            "wiki/people/sarah-chen.md"
        )));
        // Absolute paths under a layer are still content.
        assert!(Frontmatter::is_content_file(Path::new(
            "/home/db/records/companies/northstar.md"
        )));
        // index.md at any level is meta.
        assert!(!Frontmatter::is_content_file(Path::new(
            "records/contacts/index.md"
        )));
        assert!(!Frontmatter::is_content_file(Path::new("index.md")));
        // Root meta files.
        assert!(!Frontmatter::is_content_file(Path::new("DB.md")));
        assert!(!Frontmatter::is_content_file(Path::new("log.md")));
    }

    // ── effective_id ─────────────────────────────────────────────────────────

    #[test]
    fn effective_id_prefers_explicit_then_derives_from_path() {
        let with_id = Frontmatter {
            id: Some("explicit-id".into()),
            ..Default::default()
        };
        assert_eq!(
            with_id.effective_id(Path::new("wiki/people/sarah-chen.md")),
            "explicit-id"
        );
        let no_id = Frontmatter::default();
        assert_eq!(
            no_id.effective_id(Path::new("wiki/people/sarah-chen.md")),
            "sarah-chen"
        );
    }

    // ── get / set ────────────────────────────────────────────────────────────

    #[test]
    fn set_routes_universal_and_custom_keys() {
        let mut fm = Frontmatter::default();
        fm.set("type", "contact").unwrap();
        fm.set("summary", "hi").unwrap();
        fm.set("company", "[[records/companies/northstar]]")
            .unwrap();
        assert_eq!(fm.type_.as_deref(), Some("contact"));
        assert_eq!(fm.summary.as_deref(), Some("hi"));
        // Custom key landed in extra, not a typed slot.
        assert_eq!(
            fm.extra.get("company").and_then(|v| v.as_str()),
            Some("[[records/companies/northstar]]")
        );
        // get reads from both typed fields and extra.
        assert_eq!(
            fm.get("type").and_then(|v| v.as_str().map(String::from)),
            Some("contact".into())
        );
        assert_eq!(
            fm.get("company").and_then(|v| v.as_str().map(String::from)),
            Some("[[records/companies/northstar]]".into())
        );
        assert!(fm.get("nonexistent").is_none());
    }

    #[test]
    fn set_timestamp_validates_rfc3339() {
        let mut fm = Frontmatter::default();
        fm.set("created", "2026-05-27T08:00:00-07:00").unwrap();
        assert!(fm.created.is_some());
        let err = fm.set("updated", "not-a-date").unwrap_err();
        assert!(matches!(err, ParseError::BadTimestamp { .. }));
    }

    // ── extract_wiki_links ───────────────────────────────────────────────────

    #[test]
    fn extract_wiki_links_flags_full_path_short_form_and_extension() {
        let body = "See [[records/contacts/sarah-chen]] and [[sarah-chen]].\nAlso [[wiki/people/sarah-chen.md|Sarah]].\n";
        let links = extract_wiki_links(body, Path::new("doc.md"));
        assert_eq!(links.len(), 3);

        // Full path, no extension, no display.
        assert_eq!(links[0].target, "records/contacts/sarah-chen");
        assert!(links[0].is_full_path);
        assert!(!links[0].has_md_extension);
        assert_eq!(links[0].display, None);
        assert_eq!(links[0].location.1, 1, "first link on line 1");

        // Short form: not a full path.
        assert_eq!(links[1].target, "sarah-chen");
        assert!(!links[1].is_full_path, "bare target is short-form");

        // Full path WITH .md extension and a display override on line 2.
        assert_eq!(links[2].target, "wiki/people/sarah-chen.md");
        assert!(links[2].is_full_path);
        assert!(links[2].has_md_extension);
        assert_eq!(links[2].display.as_deref(), Some("Sarah"));
        assert_eq!(links[2].location.1, 2);
    }

    #[test]
    fn extract_wiki_links_reports_1_based_column_counting_chars() {
        // A multi-byte prefix (é is 2 bytes) must not skew the char column.
        let body = "café [[records/x/y]]";
        let links = extract_wiki_links(body, Path::new("d.md"));
        assert_eq!(links.len(), 1);
        // "café " is 5 chars, so the `[[` starts at char column 6 (1-based).
        assert_eq!(links[0].location.2, 6);
    }

    #[test]
    fn extract_wiki_links_ignores_a_lone_path_without_brackets() {
        let links = extract_wiki_links(
            "records/contacts/sarah-chen is not a link",
            Path::new("d.md"),
        );
        assert!(links.is_empty());
    }

    // ── extract_markdown_links ───────────────────────────────────────────────

    #[test]
    fn extract_markdown_links_captures_external_and_not_wiki_links() {
        let body =
            "See [the thread](https://x.com/a) and [[records/contacts/sarah-chen]] internally.\n";
        let md = extract_markdown_links(body, Path::new("d.md"));
        assert_eq!(
            md.len(),
            1,
            "wiki-link must not be captured as a markdown link"
        );
        assert_eq!(md[0].text, "the thread");
        assert_eq!(md[0].url, "https://x.com/a");
        assert_eq!(md[0].location.1, 1);

        // And the wiki-link extractor must not pick up the markdown link.
        let wl = extract_wiki_links(body, Path::new("d.md"));
        assert_eq!(wl.len(), 1);
        assert_eq!(wl[0].target, "records/contacts/sarah-chen");
    }

    // ── link_fields ──────────────────────────────────────────────────────────

    #[test]
    fn link_fields_extracts_scalar_list_and_summary_links() {
        // The canonical list form quotes each item so YAML parses it as clean
        // strings; a scalar field may be quoted OR written in the canonical
        // unquoted inline form `company: [[x]]` (SPEC § Linking).
        let yaml = "type: meeting\nsummary: with [[records/contacts/elena]]\ncompany: \"[[records/companies/northstar]]\"\nattendees:\n  - \"[[records/contacts/elena]]\"\n  - \"[[records/contacts/sarah]]\"\nnotes: just plain text";
        let fm = Frontmatter::parse(yaml, Path::new("m.md")).unwrap();
        // Sanity: company really did parse as a scalar string here.
        assert!(fm.extra.get("company").and_then(|v| v.as_str()).is_some());
        let fields = fm.link_fields();

        // company (scalar) once, with the right target.
        let company: Vec<&str> = fields
            .iter()
            .filter(|(k, _)| k == "company")
            .map(|(_, l)| l.target.as_str())
            .collect();
        assert_eq!(company, vec!["records/companies/northstar"]);
        // attendees (block list) twice.
        let attendees: Vec<&str> = fields
            .iter()
            .filter(|(k, _)| k == "attendees")
            .map(|(_, l)| l.target.as_str())
            .collect();
        assert_eq!(
            attendees,
            vec!["records/contacts/elena", "records/contacts/sarah"]
        );
        // summary link surfaced.
        assert_eq!(fields.iter().filter(|(k, _)| k == "summary").count(), 1);
        // Plain-text field is not a link.
        assert_eq!(fields.iter().filter(|(k, _)| k == "notes").count(), 0);
    }

    #[test]
    fn link_fields_surfaces_canonical_unquoted_scalar_link() {
        // Regression: the canonical scalar wiki-link form is the *unquoted*
        // inline `company: [[records/companies/northstar]]` (SPEC § Linking).
        // YAML parses `[[x]]` as a flow-list-in-a-list (`Seq[Seq[String]]`), so
        // a naive `as_str()`-only walk drops it. link_fields() must still
        // surface exactly one link with the correct target.
        let yaml = "type: meeting\ncompany: [[records/companies/northstar]]";
        let fm = Frontmatter::parse(yaml, Path::new("m.md")).unwrap();
        // Sanity: it really did parse as the nested sequence form, NOT a string.
        assert!(fm.extra.get("company").and_then(|v| v.as_str()).is_none());

        let fields = fm.link_fields();
        let links: Vec<(&str, &str, Option<&str>)> = fields
            .iter()
            .map(|(k, l)| (k.as_str(), l.target.as_str(), l.display.as_deref()))
            .collect();
        assert_eq!(
            links,
            vec![("company", "records/companies/northstar", None)]
        );

        // The `|display` segment survives the unquoted inline form too.
        let fm2 = Frontmatter::parse(
            "type: meeting\ncompany: [[records/companies/northstar|Northstar]]",
            Path::new("m.md"),
        )
        .unwrap();
        let f2 = fm2.link_fields();
        assert_eq!(f2.len(), 1);
        assert_eq!(f2[0].0, "company");
        assert_eq!(f2[0].1.target, "records/companies/northstar");
        assert_eq!(f2[0].1.display.as_deref(), Some("Northstar"));
    }

    #[test]
    fn link_fields_ignores_plain_one_item_flow_list() {
        // A plain one-item flow list `aliases: [foo]` parses to `Seq[String]`
        // — one nesting level shallower than an unquoted `[[foo]]` — and must
        // NOT be mistaken for a wiki-link.
        let yaml = "type: contact\naliases: [foo]";
        let fm = Frontmatter::parse(yaml, Path::new("c.md")).unwrap();
        assert_eq!(fm.link_fields(), Vec::new());
    }

    // ── detect_flow_form_link_lists ──────────────────────────────────────────

    #[test]
    fn detect_flow_form_flags_list_misencodings_not_scalars() {
        // The flow-form list mis-encoding (triple-nested) IS flagged; a scalar
        // inline wiki-link (double-nested) is NOT.
        let bad = "attendees: [[[records/x]], [[records/y]]]\nscalar_inline: [[records/z]]";
        let flagged = detect_flow_form_link_lists(bad);
        assert_eq!(flagged, vec!["attendees".to_string()]);

        // An UNquoted block list is also a mis-encoding (parses triple-nested).
        let unquoted_block = "attendees:\n  - [[records/x]]\n  - [[records/y]]";
        assert_eq!(
            detect_flow_form_link_lists(unquoted_block),
            vec!["attendees".to_string()]
        );

        // The canonical QUOTED block form parses to clean strings — NOT flagged.
        let good = "attendees:\n  - \"[[records/x]]\"\n  - \"[[records/y]]\"";
        assert!(detect_flow_form_link_lists(good).is_empty());

        // A plain scalar list of strings is not flagged.
        let plain = "tags: [a, b, c]";
        assert!(detect_flow_form_link_lists(plain).is_empty());
    }

    // ── extract_sections ─────────────────────────────────────────────────────

    #[test]
    fn extract_sections_levels_nesting_and_boundaries() {
        let body = "intro text\n## First\nalpha\n### Sub\nbeta\n## Second\ngamma\n";
        let secs = extract_sections(body);
        let headings: Vec<(&str, u8)> =
            secs.iter().map(|s| (s.heading.as_str(), s.level)).collect();
        assert_eq!(headings, vec![("First", 2), ("Sub", 3), ("Second", 2)]);

        // "First" (H2) body extends through its H3 child, stopping at "Second".
        let first = &secs[0];
        assert!(first.body.contains("alpha"));
        assert!(first.body.contains("### Sub"));
        assert!(first.body.contains("beta"));
        assert!(!first.body.contains("Second"));

        // "Sub" (H3) stops at the next equal-or-shallower heading ("Second").
        let sub = &secs[1];
        assert!(sub.body.contains("beta"));
        assert!(!sub.body.contains("gamma"));

        // 1-based line numbers within the body.
        assert_eq!(first.line, 2);
        assert_eq!(secs[2].line, 6);
    }

    #[test]
    fn extract_sections_ignores_headings_in_fenced_code() {
        let body = "## Real\n```\n## Fake heading in code\n```\nafter\n";
        let secs = extract_sections(body);
        assert_eq!(secs.len(), 1);
        assert_eq!(secs[0].heading, "Real");
        // The fenced "## Fake" is part of Real's body, not its own section.
        assert!(secs[0].body.contains("## Fake heading in code"));
    }

    // ── parse_field_spec ─────────────────────────────────────────────────────

    #[test]
    fn parse_field_spec_required_and_shape() {
        let f = parse_field_spec("- email (required, email)");
        assert_eq!(f.name, "email");
        assert!(f.required);
        assert_eq!(f.shape, Some(Shape::Email));
        assert!(f.unknown_modifiers.is_empty());
    }

    #[test]
    fn parse_field_spec_link_prefix_strips_trailing_slash() {
        let f = parse_field_spec("- company (required, link to records/companies/)");
        assert!(f.required);
        assert_eq!(f.link_prefix, Some(PathBuf::from("records/companies")));
        assert_eq!(f.shape, None);
    }

    #[test]
    fn parse_field_spec_default_preserves_case_and_value() {
        let f = parse_field_spec("- currency (default USD)");
        assert_eq!(f.name, "currency");
        assert_eq!(f.default, Some(Value::String("USD".into())));
    }

    #[test]
    fn parse_field_spec_enum_captures_comma_list_as_last_modifier() {
        let f = parse_field_spec("- status (required, enum: open, closed, pending)");
        assert!(f.required);
        assert_eq!(
            f.enum_values,
            Some(vec![
                "open".to_string(),
                "closed".to_string(),
                "pending".to_string()
            ])
        );
    }

    #[test]
    fn parse_field_spec_bare_enum_keyword_is_not_itself_a_value() {
        // `enum` with no colon: the values are the remaining tokens; the keyword
        // itself must NOT leak in as an allowed value.
        let f = parse_field_spec("- status (required, enum, open, closed)");
        assert!(f.required);
        assert_eq!(
            f.enum_values,
            Some(vec!["open".to_string(), "closed".to_string()])
        );
    }

    #[test]
    fn parse_field_spec_unknown_modifier_is_captured_not_errored() {
        let f = parse_field_spec("- weird (required, frobnicate, string)");
        assert!(f.required);
        assert_eq!(f.shape, Some(Shape::String));
        assert_eq!(f.unknown_modifiers, vec!["frobnicate".to_string()]);
    }

    #[test]
    fn parse_field_spec_no_parens_is_freeform_optional() {
        let f = parse_field_spec("- nickname");
        assert_eq!(f.name, "nickname");
        assert!(!f.required);
        assert_eq!(f.shape, None);
        assert!(f.link_prefix.is_none());
        assert!(f.enum_values.is_none());
        assert!(f.unknown_modifiers.is_empty());
    }

    // ── parse_schema_bullet (directives) ─────────────────────────────────────

    #[test]
    fn schema_bullet_unique_single_field() {
        match parse_schema_bullet("- unique: email") {
            SchemaBullet::Unique(fields) => assert_eq!(fields, vec!["email".to_string()]),
            other => panic!("expected Unique, got {other:?}"),
        }
    }

    #[test]
    fn schema_bullet_unique_compound_trims_and_splits() {
        match parse_schema_bullet("- unique: date, amount , vendor") {
            SchemaBullet::Unique(fields) => assert_eq!(
                fields,
                vec![
                    "date".to_string(),
                    "amount".to_string(),
                    "vendor".to_string()
                ]
            ),
            other => panic!("expected Unique, got {other:?}"),
        }
    }

    #[test]
    fn schema_bullet_summary_template_keeps_braces_and_inner_colons() {
        match parse_schema_bullet("- summary_template: {role} at {company} (x: y)") {
            SchemaBullet::SummaryTemplate(t) => assert_eq!(t, "{role} at {company} (x: y)"),
            other => panic!("expected SummaryTemplate, got {other:?}"),
        }
    }

    #[test]
    fn schema_bullet_field_with_enum_modifier_is_not_a_directive() {
        // A field whose modifiers contain a colon (`enum:`) parses as a field, not
        // a directive — its head has a `(` before any `:`.
        match parse_schema_bullet("- status (enum: open, closed)") {
            SchemaBullet::Field(f) => {
                assert_eq!(f.name, "status");
                assert_eq!(
                    f.enum_values,
                    Some(vec!["open".to_string(), "closed".to_string()])
                );
            }
            other => panic!("expected Field, got {other:?}"),
        }
    }

    #[test]
    fn parse_db_md_schema_captures_unique_and_summary_template() {
        let db = "---\ntype: db-md\nscope: x\nowner: y\n---\n\n## Schemas\n\n### contact\n- email (required, email)\n- unique: email\n- summary_template: {role} at {company}\n";
        let config = parse_db_md(db, Path::new("DB.md")).unwrap();
        let s = config.schemas.get("contact").expect("contact schema");
        assert_eq!(s.fields.len(), 1, "directives are not parsed as fields");
        assert_eq!(s.unique_keys, vec![vec!["email".to_string()]]);
        assert_eq!(s.summary_template.as_deref(), Some("{role} at {company}"));
    }

    #[test]
    fn schema_bullet_shard_directive_parses_values() {
        assert!(matches!(
            parse_schema_bullet("- shard: by-date"),
            SchemaBullet::Shard(Some(true))
        ));
        assert!(matches!(
            parse_schema_bullet("- shard: flat"),
            SchemaBullet::Shard(Some(false))
        ));
        // An unrecognized value is ignored (None), like an unknown modifier.
        assert!(matches!(
            parse_schema_bullet("- shard: weekly"),
            SchemaBullet::Shard(None)
        ));
        // A field whose name has a `(` before any `:` is still a field — the same
        // guard that keeps `- status (enum: a, b)` a field, not a directive.
        assert!(matches!(
            parse_schema_bullet("- shardiness (string)"),
            SchemaBullet::Field(_)
        ));
    }

    #[test]
    fn parse_db_md_schema_captures_shard_directive() {
        let db = "---\ntype: db-md\nscope: x\nowner: y\n---\n\n## Schemas\n\n### shipment\n- carrier (string)\n- shard: by-date\n\n### contact\n- shard: flat\n";
        let config = parse_db_md(db, Path::new("DB.md")).unwrap();
        let shipment = config.schemas.get("shipment").expect("shipment schema");
        assert_eq!(shipment.shard, Some(true));
        assert_eq!(
            shipment.fields.len(),
            1,
            "`shard:` is a directive, not a field"
        );
        assert_eq!(config.schemas.get("contact").unwrap().shard, Some(false));
    }

    // ── parse_db_md ──────────────────────────────────────────────────────────

    const CANONICAL_DB_MD: &str = "---\ntype: db-md\nscope: company\nowner: Sarah Chen\n---\n\n# Acme operations knowledge base\n\nCompany-scale institutional memory for Acme.\n\n## Agent instructions\n\nPrioritize creating `contact` records from new-sender emails. Use British English.\n\n## Policies\n\n### Frozen pages\n- `records/decisions/2026-q1-strategy.md` — finalized, do not modify.\n- `wiki/synthesis/2026-annual-plan.md` — signed-off plan.\n\n### Ignored types\n- `test`, `temp` — read but never synthesize.\n\n## Schemas\n\n### contact\n- name (required)\n- email (required, email)\n- company (required, link to records/companies/)\n- role (string)\n\n### expense\n- date (required, date)\n- amount (required)\n- currency (default USD)\n";

    #[test]
    fn parse_db_md_extracts_all_canonical_sections() {
        let config = parse_db_md(CANONICAL_DB_MD, Path::new("DB.md")).unwrap();

        // Agent instructions: free-form prose, heading line stripped.
        let ai = config
            .agent_instructions
            .expect("agent instructions present");
        assert!(ai.starts_with("Prioritize creating"));
        assert!(!ai.contains("## Agent instructions"));

        // Frozen pages: paths extracted from backticked bullets, comments dropped.
        assert_eq!(
            config.frozen_pages,
            vec![
                PathBuf::from("records/decisions/2026-q1-strategy.md"),
                PathBuf::from("wiki/synthesis/2026-annual-plan.md"),
            ]
        );

        // Ignored types: comma list, backticks/comment stripped.
        assert_eq!(
            config.ignored_types,
            vec!["test".to_string(), "temp".to_string()]
        );

        // Schemas: two types, each with its fields in source order.
        assert_eq!(config.schemas.len(), 2);
        let contact = config.schemas.get("contact").expect("contact schema");
        let names: Vec<&str> = contact.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["name", "email", "company", "role"]);
        assert!(contact.fields[0].required); // name
        assert_eq!(contact.fields[1].shape, Some(Shape::Email)); // email
        assert_eq!(
            contact.fields[2].link_prefix,
            Some(PathBuf::from("records/companies"))
        ); // company

        let expense = config.schemas.get("expense").expect("expense schema");
        let cur = expense
            .fields
            .iter()
            .find(|f| f.name == "currency")
            .unwrap();
        assert_eq!(cur.default, Some(Value::String("USD".into())));
    }

    #[test]
    fn parse_db_md_handles_malformed_and_unknown_modifiers() {
        // corpus-b shape: a `## Schemas` section with a malformed bullet, an
        // unknown modifier, and bullets that appear with NO `### <type>`
        // heading (so they belong to no schema and are dropped).
        let text = "---\ntype: db-md\n---\n\n## Schemas\n- orphan (required)\n\n### ticket\n- priority (required, mystery, enum: low, high)\n- broken (\n";
        let config = parse_db_md(text, Path::new("DB.md")).unwrap();

        // The orphan bullet under `## Schemas` with no `### type` heading is not
        // captured as a schema.
        assert_eq!(config.schemas.len(), 1);
        let ticket = config.schemas.get("ticket").expect("ticket schema");
        assert_eq!(ticket.fields.len(), 2);

        let priority = &ticket.fields[0];
        assert!(priority.required);
        assert_eq!(priority.unknown_modifiers, vec!["mystery".to_string()]);
        assert_eq!(
            priority.enum_values,
            Some(vec!["low".to_string(), "high".to_string()])
        );

        // A bullet with an unclosed paren still yields a usable name.
        let broken = &ticket.fields[1];
        assert_eq!(broken.name, "broken");
    }

    #[test]
    fn parse_db_md_missing_frontmatter_errors() {
        let text = "# No frontmatter\n\n## Agent instructions\nhi\n";
        let err = parse_db_md(text, Path::new("DB.md")).unwrap_err();
        assert!(matches!(err, ParseError::MissingFrontmatter { .. }));
    }

    #[test]
    fn parse_db_md_absent_sections_default_empty() {
        let text = "---\ntype: db-md\n---\n\n# Title only\n";
        let config = parse_db_md(text, Path::new("DB.md")).unwrap();
        assert_eq!(config, Config::default());
    }

    // ── fm set / --fm list-valued link fields (meeting.attendees & friends) ──

    /// `Frontmatter::set` is the value path every write surface (`fm set`,
    /// `write --fm`) funnels through. A list-of-wiki-links value (the SPEC's
    /// `meeting.attendees` shape) must serialize as a YAML **block sequence** of
    /// quoted links — readable back by [`links_in_field_value`] and accepted by
    /// `dbmd validate` — never the flow-form scalar string that trips
    /// `WIKI_LINK_FLOW_FORM_LIST`. Both the unquoted (`[[[a]], [[b]]]`) and
    /// quoted (`["[[a]]", "[[b]]"]`) spellings an agent types must normalize.
    #[test]
    fn set_list_of_wiki_links_becomes_block_sequence_both_spellings() {
        for value in [
            "[[[records/contacts/a]], [[records/contacts/b]]]",
            r#"["[[records/contacts/a]]", "[[records/contacts/b]]"]"#,
        ] {
            let mut fm = Frontmatter::default();
            fm.set("attendees", value).unwrap();

            // Stored as a 2-element sequence of clean quoted links.
            let stored = fm.extra.get("attendees").expect("attendees set");
            let Value::Sequence(items) = stored else {
                panic!("attendees must be a Sequence, got {stored:?} for input {value}");
            };
            assert_eq!(items.len(), 2, "input {value}");
            assert_eq!(items[0], Value::String("[[records/contacts/a]]".into()));
            assert_eq!(items[1], Value::String("[[records/contacts/b]]".into()));

            // The edge enumerator reads exactly the two links back (no stray
            // bracket targets, the flow-form-string symptom).
            let links: Vec<_> = links_in_field_value(stored)
                .into_iter()
                .map(|l| l.target)
                .collect();
            assert_eq!(
                links,
                vec!["records/contacts/a", "records/contacts/b"],
                "input {value}"
            );

            // And the canonical writer renders it block-style, not as a scalar.
            let yaml = fm.to_yaml();
            assert!(
                yaml.contains("attendees:\n"),
                "expected block list in:\n{yaml}"
            );
            assert!(
                !yaml.contains("attendees: '[["),
                "must not be a flow-form scalar string in:\n{yaml}"
            );
        }
    }

    /// A *single* inline wiki-link stays a scalar string (renders inline
    /// `field: [[x]]`), and a single link must never be widened to a one-item
    /// list — preserving the common `contact.company` / `expense.vendor` shape.
    #[test]
    fn set_single_inline_wiki_link_stays_scalar() {
        let mut fm = Frontmatter::default();
        fm.set("company", "[[records/companies/tideform]]").unwrap();
        assert_eq!(
            fm.extra.get("company"),
            Some(&Value::String("[[records/companies/tideform]]".into())),
        );
        // Still recognized as one link.
        let links: Vec<_> = links_in_field_value(fm.extra.get("company").unwrap())
            .into_iter()
            .map(|l| l.target)
            .collect();
        assert_eq!(links, vec!["records/companies/tideform"]);
    }

    /// Plain text and a non-link flow list are left as verbatim scalar strings —
    /// the list normalization only triggers when every item is a clean wiki-link.
    #[test]
    fn set_non_link_values_stay_scalar_strings() {
        let mut fm = Frontmatter::default();
        fm.set("location", "Video call (remote)").unwrap();
        assert_eq!(
            fm.extra.get("location"),
            Some(&Value::String("Video call (remote)".into())),
        );

        // A flow list whose items are NOT wiki-links must not be reinterpreted as
        // a link sequence; it stays the scalar string the agent passed.
        fm.set("note", "[draft, wip]").unwrap();
        assert_eq!(
            fm.extra.get("note"),
            Some(&Value::String("[draft, wip]".into()))
        );
    }
}
