//! gen-scale.rs — deterministic generator for `corpus-d-scale`, the db.md
//! performance corpus (plan: db-md-rust-toolkit.md, Block 6, line 494).
//!
//! WHAT IT BUILDS
//! --------------
//! A complete, valid db.md store across all three layers — `sources/`,
//! `records/`, `wiki/` — sized by a tier:
//!
//!   * `10k`  (default, CI tier)        ≈ 10,000 content files
//!   * `1m`   (opt-in, nightly tier)    ≈ 1,000,000 content files
//!
//! It exercises every scale property the toolkit's budgets are measured
//! against:
//!
//!   * Date-sharded sources across MULTIPLE MONTHS — `sources/<type>/<YYYY>/<MM>/`
//!     (emails + transcripts + docs), so a type-folder index aggregates
//!     across shards and only the current shard is "hot".
//!   * Event-driven RECORDS also date-shard (`records/expenses/<YYYY>/<MM>/`,
//!     `records/invoices/...`, `records/meetings/...`) — sharding is a
//!     property of the TYPE, not the layer (SPEC § Scale).
//!   * Entity records (`records/contacts/`, `records/companies/`) and all of
//!     `wiki/` stay FLAT — dedup-/curation-bounded, never overflow a dir.
//!   * An OVERFLOW type-folder (`sources/emails/`, > 500 files spread across
//!     shards): its `index.md` caps at exactly 500 entries (newest first by
//!     the frontmatter `updated` field, ties broken by store-relative path
//!     ascending) and ends with a `## More` footer, while its `index.jsonl`
//!     lists ALL N files. This is the cap-vs-complete invariant.
//!   * A MULTI-MONTH log: the active `log.md` holds only the current month;
//!     older months are rotated into `log/<YYYY-MM>.md` archives.
//!
//! The full `index.md` (root + per-layer + per-type-folder, capped 500 by
//! recency) and the complete `index.jsonl` twin (per type-folder, uncapped)
//! are generated for every non-empty folder, so the corpus is `dbmd validate
//! --all`-clean on the index hierarchy out of the box and is a fixed point of
//! `dbmd index rebuild` (modulo the documented overflow truncation of
//! `index.md`).
//!
//! HARD-RULE COMPLIANCE
//! --------------------
//!   * Pure Rust, std-only. ZERO dependencies — no serde, no chrono, no AI/LLM
//!     crate, no embeddings/vectors. YAML, JSON and JSONL are emitted by hand.
//!     That keeps this generator off the shipped bundle's dependency/license
//!     surface entirely; it is a test tool, not part of `dbmd`.
//!   * Wiki-links are always FULL store-relative paths (`[[records/...]]`),
//!     never short forms.
//!   * Deterministic: a seeded LCG drives every choice, so the same tier
//!     always produces a byte-identical store (reproducible benchmarks, stable
//!     fixtures).
//!
//! HOW TO RUN
//! ----------
//! It is a standalone program, NOT a Cargo workspace member (keeping it out of
//! the `dbmd` bundle). Compile with `rustc` and run:
//!
//!   # 10k tier (CI default) — fast, a few seconds:
//!   rustc -O tests/gen-scale.rs -o /tmp/gen-scale
//!   /tmp/gen-scale 10k tests/corpora/corpus-d-scale
//!
//!   # 1M tier (opt-in scale job) — minutes + a few GB of disk; NOT for CI:
//!   /tmp/gen-scale 1m /path/to/big/corpus-d-scale-1m
//!
//! Args: `gen-scale <tier> <out-dir>`
//!   <tier>     one of `10k` (default) or `1m`
//!   <out-dir>  destination directory; created if absent. If it already exists
//!              the run aborts unless `--force` is passed (then it is wiped).
//!
//! Flags:
//!   --force    remove <out-dir> first if it exists
//!   --seed N   override the PRNG seed (default 0xD8_2026)
//!
//! Convenience: a tiny `tests/corpora/Makefile`-free path — from the repo root
//!   cargo? no. Just: `rustc -O tests/gen-scale.rs -o target/gen-scale && \
//!                      target/gen-scale 10k tests/corpora/corpus-d-scale --force`

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::exit;

// ───────────────────────────── tiers ──────────────────────────────────────

/// A tier is a target store size plus the per-type counts that compose it.
/// Counts are chosen so the layer mix is realistic for a COMPANY brain (event
/// records rival sources in volume — see the plan's "records add only a few %
/// is false for a company" note) and so `sources/emails/` always overflows the
/// 500-cap regardless of tier.
struct Tier {
    name: &'static str,
    // sources (all date-sharded across `months`)
    emails: usize,      // the overflow type-folder — always > 500
    transcripts: usize, // a second sharded source type
    docs: usize,        // pdf-source, sharded
    // records — entity types (FLAT, dedup-bounded)
    contacts: usize,
    companies: usize,
    // records — event types (date-sharded, track business volume)
    expenses: usize,
    invoices: usize,
    meetings: usize,
    // wiki (FLAT, curation-bounded)
    wiki_people: usize,
    wiki_projects: usize,
    // how many months the date-shards (and the log) span
    months: usize,
}

impl Tier {
    fn parse(s: &str) -> Option<Tier> {
        match s {
            "10k" => Some(Tier {
                name: "10k",
                // sources ≈ 5,200 (emails overflow well past 500)
                emails: 3_000,
                transcripts: 1_200,
                docs: 1_000,
                // entity records ≈ 800 (flat, bounded)
                contacts: 500,
                companies: 300,
                // event records ≈ 3,900 (sharded, business volume)
                expenses: 2_000,
                invoices: 900,
                meetings: 1_000,
                // wiki ≈ 100 (flat, curation-bounded)
                wiki_people: 60,
                wiki_projects: 40,
                months: 6,
                // total ≈ 10,000 content files
            }),
            "1m" => Some(Tier {
                name: "1m",
                // sources ≈ 600k
                emails: 400_000,
                transcripts: 120_000,
                docs: 80_000,
                // entity records ≈ 60k (still flat — the dedup-bounded set just
                // happens to be large at company scale; exercises a big flat dir)
                contacts: 40_000,
                companies: 20_000,
                // event records ≈ 335k (sharded)
                expenses: 200_000,
                invoices: 60_000,
                meetings: 75_000,
                // wiki ≈ 5k (flat)
                wiki_people: 3_000,
                wiki_projects: 2_000,
                months: 36,
                // total ≈ 1,000,000 content files
            }),
            _ => None,
        }
    }

    fn total(&self) -> usize {
        self.emails
            + self.transcripts
            + self.docs
            + self.contacts
            + self.companies
            + self.expenses
            + self.invoices
            + self.meetings
            + self.wiki_people
            + self.wiki_projects
    }
}

// ───────────────────────── deterministic PRNG ─────────────────────────────

/// SplitMix64 — a tiny, std-only, deterministic PRNG. We only need stable
/// pseudo-random choices, not cryptographic quality. Same seed ⇒ same store.
struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng { state: seed }
    }
    fn next_u64(&mut self) -> u64 {
        // SplitMix64
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in [0, n). n must be > 0.
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % (n as u64)) as usize
    }
    /// True with probability num/den.
    fn chance(&mut self, num: u64, den: u64) -> bool {
        self.next_u64() % den < num
    }
}

// ───────────────────────────── word banks ─────────────────────────────────

const FIRST_NAMES: &[&str] = &[
    "sarah", "elena", "marcus", "priya", "james", "yuki", "diego", "amara",
    "noah", "ingrid", "omar", "lena", "tomas", "fatima", "kai", "rosa",
    "viktor", "aisha", "liam", "mei", "andre", "nadia", "sven", "leila",
    "hassan", "clara", "raj", "freya", "pablo", "zoe", "kenji", "amina",
];
const LAST_NAMES: &[&str] = &[
    "chen", "rodriguez", "okafor", "patel", "nguyen", "tanaka", "silva",
    "okeke", "johansson", "ali", "muller", "santos", "kim", "haddad",
    "novak", "abara", "schmidt", "costa", "ivanov", "rahman", "lindqvist",
    "mendez", "yamamoto", "diallo", "petrov", "khan", "berg", "moreau",
];
const COMPANY_STEMS: &[&str] = &[
    "northstar", "acme", "helios", "meridian", "atlas", "verdant", "cobalt",
    "lumen", "quanta", "ironwood", "stratus", "beacon", "tessera", "vector",
    "nimbus", "axion", "polaris", "fathom", "kestrel", "obsidian", "sable",
    "aurora", "talon", "cinder", "marrow", "quill", "harbor", "summit",
];
const COMPANY_SUFFIX: &[&str] = &["io", "co", "inc", "labs", "group", "systems"];
const INDUSTRIES: &[&str] = &[
    "logistics", "fintech", "healthcare", "retail", "manufacturing",
    "media", "education", "energy", "biotech", "real-estate",
];
const RELATIONSHIPS: &[&str] = &["customer", "vendor", "partner", "prospect"];
const ROLES: &[&str] = &[
    "Director of Operations", "VP Engineering", "Account Manager",
    "Chief of Staff", "Head of Finance", "Procurement Lead", "CTO",
    "Office Manager", "Founder", "Operations Analyst",
];
const EXPENSE_CATEGORIES: &[&str] = &[
    "software", "travel", "hardware", "meals", "marketing", "contractors",
    "office", "subscriptions", "utilities", "events",
];
const INVOICE_STATUS: &[&str] = &["paid", "unpaid", "void"];
const SUBJECTS: &[&str] = &[
    "Renewal discussion", "Quarterly review", "Contract amendment",
    "Invoice query", "Onboarding next steps", "Seat expansion",
    "Pricing question", "Support escalation", "Partnership proposal",
    "Kickoff scheduling", "Budget approval", "Vendor evaluation",
];
const PROJECT_NAMES: &[&str] = &[
    "renewal-program", "q2-expansion", "vendor-consolidation",
    "ops-automation", "data-migration", "cost-reduction", "rebrand",
    "platform-rollout", "compliance-audit", "supply-chain-review",
];
const LANGUAGES: &[&str] = &["en", "en-GB", "es", "pt", "ja"];
const DOC_TYPES: &[&str] = &["contract", "report", "invoice-scan", "spec", "proposal"];

// ─────────────────────────── date helpers ─────────────────────────────────

/// A simple (year, month, day, hour, minute) tuple. We anchor the corpus at a
/// fixed "now" and walk backwards by month for shards/log so output is stable.
#[derive(Clone, Copy)]
struct Stamp {
    y: i32,
    mo: u32,
    d: u32,
    h: u32,
    mi: u32,
}

impl Stamp {
    /// RFC3339 in UTC, e.g. `2026-05-27T10:00:00Z`. db.md timestamps are
    /// ISO-8601; UTC `Z` form keeps them clone-stable and trivially comparable.
    fn rfc3339(&self) -> String {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:00Z",
            self.y, self.mo, self.d, self.h, self.mi
        )
    }
    /// Date-only `YYYY-MM-DD` (for `date:` / `first_touch:` style fields).
    fn date(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.y, self.mo, self.d)
    }
    /// `YYYY-MM` shard / log-archive key.
    fn ym(&self) -> String {
        format!("{:04}-{:02}", self.y, self.mo)
    }
    /// `YYYY` and `MM` shard path segments.
    fn year(&self) -> String {
        format!("{:04}", self.y)
    }
    fn month(&self) -> String {
        format!("{:02}", self.mo)
    }
    /// `[YYYY-MM-DD HH:MM]` — the log entry header timestamp form.
    fn log_ts(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            self.y, self.mo, self.d, self.h, self.mi
        )
    }
}

/// Days in month (no leap-year edge needed for the months we anchor on, but
/// handle it anyway so any anchor works).
fn days_in_month(y: i32, mo: u32) -> u32 {
    match mo {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

/// The fixed "current" month the corpus is anchored at. The active `log.md`
/// holds this month; everything older rotates to `log/<YYYY-MM>.md`.
const ANCHOR_YEAR: i32 = 2026;
const ANCHOR_MONTH: u32 = 5;

/// The (year, month) that is `back` months before the anchor (back=0 ⇒ anchor).
fn month_back(back: usize) -> (i32, u32) {
    let total = (ANCHOR_YEAR as i64) * 12 + (ANCHOR_MONTH as i64 - 1) - back as i64;
    let y = (total.div_euclid(12)) as i32;
    let mo = (total.rem_euclid(12)) as u32 + 1;
    (y, mo)
}

// ─────────────────────── frontmatter / file writers ───────────────────────

/// A content file the generator is about to write. We collect the frontmatter
/// fields in insertion order (a Vec of (key, RenderedValue)) plus tags/links so
/// the same struct feeds BOTH the on-disk file AND its `index.jsonl` entry —
/// the two never drift because they share one source.
struct Doc {
    /// store-relative path WITHOUT extension, e.g. `records/contacts/sarah-chen`
    rel: String,
    type_: String,
    summary: String,
    created: String,
    updated: String,
    /// ordered scalar frontmatter fields (key, raw-yaml-value-string).
    /// Values are already rendered: strings are quoted, wiki-links are bare
    /// `[[...]]`, dates are bare. `tags` and link-list fields are NOT here —
    /// they are handled separately so JSON and YAML stay consistent.
    fields: Vec<(String, String)>,
    /// flat semantic labels (the doc-side tag expansion)
    tags: Vec<String>,
    /// wiki-link targets carried in the body / link fields (full paths, no `.md`)
    links: Vec<String>,
    /// the markdown body (after frontmatter). May contain `[[...]]` wiki-links.
    body: String,
    /// any list-valued link fields, rendered as YAML block sequences in the
    /// frontmatter (key -> list of full-path targets). e.g. meeting attendees.
    link_lists: Vec<(String, Vec<String>)>,
}

impl Doc {
    /// Write the markdown file to disk under `store_root`, creating parent dirs.
    fn write_md(&self, store_root: &Path) -> io::Result<()> {
        let path = store_root.join(format!("{}.md", self.rel));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let f = fs::File::create(&path)?;
        let mut w = BufWriter::new(f);
        w.write_all(b"---\n")?;
        write!(w, "type: {}\n", self.type_)?;
        write!(w, "created: {}\n", self.created)?;
        write!(w, "updated: {}\n", self.updated)?;
        write!(w, "summary: {}\n", yaml_quote(&self.summary))?;
        for (k, v) in &self.fields {
            write!(w, "{}: {}\n", k, v)?;
        }
        for (k, items) in &self.link_lists {
            write!(w, "{}:\n", k)?;
            for it in items {
                write!(w, "  - [[{}]]\n", it)?;
            }
        }
        if !self.tags.is_empty() {
            write!(w, "tags: [{}]\n", self.tags.join(", "))?;
        }
        // The universal `status` lifecycle field — but only if a type-specific
        // field hasn't already claimed the `status` key (e.g. `invoice.status`
        // is the paid/unpaid/void enum and IS the status). Emitting both would
        // be a duplicate YAML key (caught by dbmd-core's parser).
        let has_status = self.fields.iter().any(|(k, _)| k == "status");
        if !has_status {
            w.write_all(b"status: active\n")?;
        }
        w.write_all(b"---\n\n")?;
        w.write_all(self.body.as_bytes())?;
        if !self.body.ends_with('\n') {
            w.write_all(b"\n")?;
        }
        w.flush()?;
        Ok(())
    }

    /// Render this doc's complete `index.jsonl` entry as one JSON object on one
    /// line: `{path, type, summary, tags, links, created, updated, <fields>}`.
    /// Hand-rolled JSON (no serde dep). All scalar `fields` are emitted as JSON
    /// strings (with their YAML quoting stripped); link-list fields are emitted
    /// as JSON arrays of the bare target paths.
    fn jsonl_entry(&self) -> String {
        let mut o = String::with_capacity(256);
        o.push('{');
        json_kv_str(&mut o, "path", &format!("{}.md", self.rel), true);
        json_kv_str(&mut o, "type", &self.type_, false);
        json_kv_str(&mut o, "summary", &self.summary, false);
        // tags array
        o.push_str(",\"tags\":");
        json_arr(&mut o, &self.tags);
        // links array
        o.push_str(",\"links\":");
        json_arr(&mut o, &self.links);
        json_kv_str(&mut o, "created", &self.created, false);
        json_kv_str(&mut o, "updated", &self.updated, false);
        // Universal `status`: mirror what the .md carries — the type-specific
        // value when a field named `status` exists (invoices), else `active`.
        if !self.fields.iter().any(|(k, _)| k == "status") {
            json_kv_str(&mut o, "status", "active", false);
        }
        // remaining scalar fields, in order
        for (k, v) in &self.fields {
            let plain = unwrap_yaml_scalar(v);
            o.push(',');
            json_str(&mut o, k);
            o.push(':');
            json_str(&mut o, &plain);
        }
        // link-list fields as arrays
        for (k, items) in &self.link_lists {
            o.push(',');
            json_str(&mut o, k);
            o.push(':');
            json_arr(&mut o, items);
        }
        o.push('}');
        o
    }
}

/// A catalog line as it appears in a type-folder `index.md`, plus the sort key
/// (`updated` then path) used to apply the 500-cap by recency.
struct IndexEntry {
    rel: String, // store-relative, no extension (the wiki-link target)
    summary: String,
    tags: Vec<String>,
    updated: String, // RFC3339 — recency sort key (newest first)
}

// ───────────────────────── string-encoding helpers ────────────────────────

/// Quote a string for a YAML scalar value. We always double-quote `summary`
/// and string fields so embedded `:`/`#`/`[` never break the parse; escape `"`
/// and `\`. (db.md frontmatter is YAML.)
fn yaml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Strip the surrounding double-quotes (and unescape) from a value previously
/// produced by `yaml_quote`, so the JSONL twin carries the plain scalar. Bare
/// values (dates, wiki-links, numbers) pass through unchanged.
fn unwrap_yaml_scalar(v: &str) -> String {
    let b = v.as_bytes();
    if b.len() >= 2 && b[0] == b'"' && b[b.len() - 1] == b'"' {
        let inner = &v[1..v.len() - 1];
        inner.replace("\\\"", "\"").replace("\\\\", "\\")
    } else {
        v.to_string()
    }
}

/// Append a JSON string literal (with escaping) to `o`.
fn json_str(o: &mut String, s: &str) {
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            '\r' => o.push_str("\\r"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            _ => o.push(c),
        }
    }
    o.push('"');
}

/// Append `,"key":"value"` (or `"key":"value"` when `first`) to `o`.
fn json_kv_str(o: &mut String, key: &str, val: &str, first: bool) {
    if !first {
        o.push(',');
    }
    json_str(o, key);
    o.push(':');
    json_str(o, val);
}

/// Append a JSON array of strings to `o`.
fn json_arr(o: &mut String, items: &[String]) {
    o.push('[');
    for (i, it) in items.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        json_str(o, it);
    }
    o.push(']');
}

// ─────────────────────── per-type document builders ───────────────────────

/// Build a deterministic person name + slug from an index.
fn person(idx: usize, rng: &mut Rng) -> (String, String) {
    let _ = rng;
    let first = FIRST_NAMES[idx % FIRST_NAMES.len()];
    let last = LAST_NAMES[(idx / FIRST_NAMES.len()) % LAST_NAMES.len()];
    let disp = format!(
        "{}{} {}{}",
        first[..1].to_uppercase(),
        &first[1..],
        last[..1].to_uppercase(),
        &last[1..]
    );
    // slug is unique per idx (append idx to defeat collisions past the bank size)
    let slug = format!("{}-{}-{}", first, last, idx);
    (disp, slug)
}

/// Build a deterministic company name + slug + domain from an index.
fn company(idx: usize) -> (String, String, String) {
    let stem = COMPANY_STEMS[idx % COMPANY_STEMS.len()];
    let suffix = COMPANY_SUFFIX[(idx / COMPANY_STEMS.len()) % COMPANY_SUFFIX.len()];
    let slug = format!("{}-{}-{}", stem, suffix, idx);
    let disp = format!(
        "{}{} {}",
        stem[..1].to_uppercase(),
        &stem[1..],
        suffix.to_uppercase()
    );
    let domain = format!("{}-{}.{}", stem, idx, suffix);
    (disp, slug, domain)
}

/// Spread `i` of `n` items across `months` month-shards and within-month days,
/// returning a Stamp. Newer items get lower `back` (so the active month is the
/// densest, like a real store). Deterministic given (i, n).
fn stamp_for(i: usize, n: usize, months: usize, rng: &mut Rng) -> Stamp {
    // Distribute across months weighted toward recent: month index by position.
    let back = if months <= 1 {
        0
    } else {
        // i=0 is oldest, i=n-1 newest → back decreases with i
        let frac = i * months / n.max(1);
        months - 1 - frac.min(months - 1)
    };
    let (y, mo) = month_back(back);
    let dim = days_in_month(y, mo);
    let d = 1 + rng.below(dim as usize) as u32;
    let h = rng.below(24) as u32;
    let mi = rng.below(60) as u32;
    Stamp { y, mo, d, h, mi }
}

// ─────────────────────────────── main ─────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut tier_name = "10k".to_string();
    let mut out_dir: Option<String> = None;
    let mut force = false;
    let mut seed: u64 = 0x00D8_2026;

    let mut i = 1;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--force" => force = true,
            "--seed" => {
                i += 1;
                seed = args
                    .get(i)
                    .and_then(|s| {
                        if let Some(hex) = s.strip_prefix("0x") {
                            u64::from_str_radix(hex, 16).ok()
                        } else {
                            s.parse().ok()
                        }
                    })
                    .unwrap_or_else(|| die("--seed needs a number"));
            }
            "-h" | "--help" => {
                print_help();
                exit(0);
            }
            s if s.starts_with('-') => die(&format!("unknown flag: {s}")),
            s => {
                // positional: first is tier (if it parses), else out-dir
                if Tier::parse(s).is_some() && out_dir.is_none() && tier_name == "10k" {
                    tier_name = s.to_string();
                } else if out_dir.is_none() {
                    out_dir = Some(s.to_string());
                } else {
                    tier_name = s.to_string();
                }
            }
        }
        i += 1;
    }

    let tier = Tier::parse(&tier_name)
        .unwrap_or_else(|| die(&format!("unknown tier '{tier_name}' (want 10k or 1m)")));
    let out_dir = out_dir.unwrap_or_else(|| die("missing <out-dir>"));
    let out = PathBuf::from(&out_dir);

    if out.exists() {
        if force {
            fs::remove_dir_all(&out).unwrap_or_else(|e| die(&format!("cannot wipe {out_dir}: {e}")));
        } else {
            die(&format!(
                "{out_dir} already exists (pass --force to overwrite)"
            ));
        }
    }
    fs::create_dir_all(&out).unwrap_or_else(|e| die(&format!("cannot create {out_dir}: {e}")));

    eprintln!(
        "gen-scale: tier={} (~{} content files), out={}, seed=0x{:X}",
        tier.name,
        tier.total(),
        out_dir,
        seed
    );

    let mut rng = Rng::new(seed);
    let mut g = Gen {
        out: out.clone(),
        tier: &tier,
        // per-type-folder collected index entries (rel without ext → entry)
        indexes: BTreeMap::new(),
        // per-type-folder jsonl file handles' buffered contents
        jsonl: BTreeMap::new(),
        // log entries by month-key "YYYY-MM" → buffered body
        log_by_month: BTreeMap::new(),
        written: 0,
    };

    g.gen_db_md();
    g.gen_companies(&mut rng);
    g.gen_contacts(&mut rng);
    g.gen_emails(&mut rng);
    g.gen_transcripts(&mut rng);
    g.gen_docs(&mut rng);
    g.gen_expenses(&mut rng);
    g.gen_invoices(&mut rng);
    g.gen_meetings(&mut rng);
    g.gen_wiki_people(&mut rng);
    g.gen_wiki_projects(&mut rng);

    g.flush_jsonl().unwrap_or_else(|e| die(&format!("jsonl write failed: {e}")));
    g.write_all_indexes().unwrap_or_else(|e| die(&format!("index write failed: {e}")));
    g.write_log().unwrap_or_else(|e| die(&format!("log write failed: {e}")));
    g.write_gitattributes().unwrap_or_else(|e| die(&format!("gitattributes write failed: {e}")));

    eprintln!(
        "gen-scale: done — {} content files written under {}",
        g.written, out_dir
    );
}

fn print_help() {
    eprintln!(
        "gen-scale — deterministic db.md scale-corpus generator\n\
         \n\
         USAGE:\n\
         \x20 rustc -O tests/gen-scale.rs -o /tmp/gen-scale\n\
         \x20 /tmp/gen-scale <tier> <out-dir> [--force] [--seed N]\n\
         \n\
         ARGS:\n\
         \x20 <tier>     10k (default, CI) | 1m (opt-in scale job)\n\
         \x20 <out-dir>  destination store dir (created if absent)\n\
         \n\
         FLAGS:\n\
         \x20 --force    wipe <out-dir> first if it exists\n\
         \x20 --seed N   PRNG seed (default 0xD82026); decimal or 0x-hex\n"
    );
}

fn die(msg: &str) -> ! {
    eprintln!("gen-scale: error: {msg}");
    exit(2);
}

// ─────────────────────── the generator state machine ──────────────────────

struct Gen<'a> {
    out: PathBuf,
    tier: &'a Tier,
    /// type-folder (e.g. "records/contacts") → its catalog entries
    indexes: BTreeMap<String, Vec<IndexEntry>>,
    /// type-folder → buffered JSONL body (one entry per line)
    jsonl: BTreeMap<String, String>,
    /// "YYYY-MM" → buffered log body for that month
    log_by_month: BTreeMap<String, String>,
    written: usize,
}

impl<'a> Gen<'a> {
    /// Record a finished doc: write the .md, stash its index entry + jsonl line.
    fn emit(&mut self, type_folder: &str, doc: Doc) {
        doc.write_md(&self.out)
            .unwrap_or_else(|e| die(&format!("write {}: {e}", doc.rel)));
        self.indexes
            .entry(type_folder.to_string())
            .or_default()
            .push(IndexEntry {
                rel: doc.rel.clone(),
                summary: doc.summary.clone(),
                tags: doc.tags.clone(),
                updated: doc.updated.clone(),
            });
        let line = doc.jsonl_entry();
        let buf = self.jsonl.entry(type_folder.to_string()).or_default();
        buf.push_str(&line);
        buf.push('\n');
        self.written += 1;
    }

    /// Append a log entry to the right month bucket.
    fn log(&mut self, ts: &Stamp, kind: &str, object: &str, note: &str) {
        let buf = self.log_by_month.entry(ts.ym()).or_default();
        if object.is_empty() {
            buf.push_str(&format!("## [{}] {}\n", ts.log_ts(), kind));
        } else {
            buf.push_str(&format!("## [{}] {} | {}\n", ts.log_ts(), kind, object));
        }
        if !note.is_empty() {
            buf.push_str(note);
            buf.push('\n');
        }
        buf.push('\n');
    }

    // ── DB.md ──────────────────────────────────────────────────────────────

    /// The store marker + config. Mirrors corpus-a's schema vocabulary so the
    /// scale corpus validates against the same `## Schemas` rules.
    fn gen_db_md(&mut self) {
        let body = format!(
            "---\n\
             type: db-md\n\
             scope: company\n\
             owner: Sarah Chen\n\
             computer_id: acme-scale\n\
             ---\n\
             \n\
             # Acme operations knowledge base (scale corpus)\n\
             \n\
             Generated by `tests/gen-scale.rs` (tier `{tier}`). A synthetic,\n\
             company-scale db.md store used to measure the toolkit's loop-op and\n\
             sweep-op performance budgets. All three layers are populated;\n\
             sources and event records date-shard across {months} months;\n\
             `sources/emails/` overflows the 500-entry `index.md` cap (the\n\
             `index.jsonl` twin stays complete); the log spans multiple months\n\
             with older months rotated into `log/`.\n\
             \n\
             This store is regenerated, never hand-edited — it is a frozen\n\
             performance fixture, not a living store.\n\
             \n\
             ## Agent instructions\n\
             \n\
             Use British English in `wiki/` pages. When a vendor invoice\n\
             arrives, also create an `expense` record linked to the invoice.\n\
             Link each contact to its `company` record. Keep `summary` fields\n\
             one line and current.\n\
             \n\
             ## Policies\n\
             \n\
             ### Frozen pages\n\
             - `wiki/projects/renewal-program-0.md` — signed-off plan; do not modify.\n\
             \n\
             ### Ignored types\n\
             - `test` — read as ambient context but never synthesised.\n\
             \n\
             ## Schemas\n\
             \n\
             ### contact\n\
             - name (required, string)\n\
             - email (required, email)\n\
             - company (required, link to records/companies/)\n\
             - role (string)\n\
             - first_touch (date)\n\
             - last_touch (date)\n\
             \n\
             ### company\n\
             - name (required, string)\n\
             - domain (required, string)\n\
             - industry (string)\n\
             - relationship (enum: customer, vendor, partner, prospect)\n\
             \n\
             ### expense\n\
             - date (required, date)\n\
             - amount (required, currency)\n\
             - currency (default USD)\n\
             - category (string)\n\
             - vendor (required, link to records/companies/)\n\
             \n\
             ### meeting\n\
             - date (required, date)\n\
             - attendees (required, link to records/contacts/)\n\
             - location (string)\n\
             - duration_min (int)\n\
             \n\
             ### invoice\n\
             - date (required, date)\n\
             - amount (required, currency)\n\
             - vendor (required, link to records/companies/)\n\
             - status (required, enum: paid, unpaid, void)\n\
             - paid_at (date)\n",
            tier = self.tier.name,
            months = self.tier.months
        );
        fs::write(self.out.join("DB.md"), body)
            .unwrap_or_else(|e| die(&format!("write DB.md: {e}")));
    }

    // ── records/companies — FLAT entity type ────────────────────────────────

    fn gen_companies(&mut self, rng: &mut Rng) {
        let n = self.tier.companies;
        for i in 0..n {
            let (disp, slug, domain) = company(i);
            let st = stamp_for(i, n, self.tier.months, rng);
            let industry = INDUSTRIES[i % INDUSTRIES.len()];
            let rel = RELATIONSHIPS[i % RELATIONSHIPS.len()];
            let summary = format!("{rel}; {industry}");
            let body = format!(
                "# {disp}\n\n{disp} is a {rel} in {industry}. Primary domain \
                 [{domain}](https://{domain}).\n",
                disp = disp,
                rel = rel,
                industry = industry,
                domain = domain
            );
            let doc = Doc {
                rel: format!("records/companies/{slug}"),
                type_: "company".into(),
                summary,
                created: st.rfc3339(),
                updated: st.rfc3339(),
                fields: vec![
                    ("name".into(), yaml_quote(&disp)),
                    ("domain".into(), yaml_quote(&domain)),
                    ("industry".into(), yaml_quote(industry)),
                    ("relationship".into(), rel.to_string()),
                ],
                tags: vec![rel.to_string(), industry.to_string()],
                links: vec![],
                body,
                link_lists: vec![],
            };
            self.emit("records/companies", doc);
        }
    }

    // ── records/contacts — FLAT entity type, links to a company ──────────────

    fn gen_contacts(&mut self, rng: &mut Rng) {
        let n = self.tier.contacts;
        let ncomp = self.tier.companies.max(1);
        for i in 0..n {
            let (disp, slug) = person(i, rng);
            let cidx = rng.below(ncomp);
            let (cdisp, cslug, cdomain) = company(cidx);
            let company_link = format!("records/companies/{cslug}");
            let st = stamp_for(i, n, self.tier.months, rng);
            let first_touch = stamp_for(i / 2, n, self.tier.months, rng);
            let role = ROLES[i % ROLES.len()];
            let email = format!(
                "{}.{}@{}",
                slug.split('-').next().unwrap_or("x"),
                i,
                cdomain
            );
            let summary = format!("{role} at {cdisp} (last_touch: {})", st.date());
            let body = format!(
                "# {disp}\n\n{role} at [[{company_link}]]. Primary contact on \
                 the {cdisp} account.\n",
                disp = disp,
                role = role,
                company_link = company_link,
                cdisp = cdisp
            );
            let doc = Doc {
                rel: format!("records/contacts/{slug}"),
                type_: "contact".into(),
                summary,
                created: first_touch.rfc3339(),
                updated: st.rfc3339(),
                fields: vec![
                    ("name".into(), yaml_quote(&disp)),
                    ("email".into(), yaml_quote(&email)),
                    ("company".into(), format!("[[{company_link}]]")),
                    ("role".into(), yaml_quote(role)),
                    ("first_touch".into(), first_touch.date()),
                    ("last_touch".into(), st.date()),
                ],
                tags: vec!["customer".into()],
                links: vec![company_link],
                body,
                link_lists: vec![],
            };
            self.emit("records/contacts", doc);
        }
    }

    // ── sources/emails — date-sharded, the OVERFLOW type-folder ──────────────

    fn gen_emails(&mut self, rng: &mut Rng) {
        let n = self.tier.emails;
        let ncontacts = self.tier.contacts.max(1);
        for i in 0..n {
            let st = stamp_for(i, n, self.tier.months, rng);
            let (pdisp, pslug) = person(rng.below(ncontacts), rng);
            let subject = SUBJECTS[i % SUBJECTS.len()];
            let from = format!(
                "{}@external-{}.com",
                pslug.split('-').next().unwrap_or("x"),
                i % 97
            );
            let to = "ops@acme.com";
            let contact_link = format!("records/contacts/{pslug}");
            let id = format!("{}-{}-email-{}", st.ym(), subject_slug(subject), i);
            // sharded path: sources/emails/<YYYY>/<MM>/<id>
            let rel = format!("sources/emails/{}/{}/{}", st.year(), st.month(), id);
            let summary = format!("{from} → {to} — {subject}");
            let body = format!(
                "# {subject}\n\nFrom {pdisp} ({from}). Re: {subject}. See \
                 contact [[{contact_link}]].\n",
                subject = subject,
                pdisp = pdisp,
                from = from,
                contact_link = contact_link
            );
            let doc = Doc {
                rel,
                type_: "email".into(),
                summary,
                created: st.rfc3339(),
                updated: st.rfc3339(),
                fields: vec![
                    ("from".into(), yaml_quote(&from)),
                    ("to".into(), yaml_quote(to)),
                    ("date".into(), st.rfc3339()),
                    ("subject".into(), yaml_quote(subject)),
                ],
                tags: vec![subject_slug(subject)],
                links: vec![contact_link],
                body,
                link_lists: vec![],
            };
            let ingest_obj = doc.rel.clone();
            self.emit("sources/emails", doc);
            // log a slice of ingests so the log spans months without being huge
            if rng.chance(1, 40) {
                self.log(&st, "ingest", &format!("[[{}]]", ingest_obj), "Email ingested.");
            }
        }
    }

    // ── sources/transcripts — date-sharded second source type ────────────────

    fn gen_transcripts(&mut self, rng: &mut Rng) {
        let n = self.tier.transcripts;
        let ncontacts = self.tier.contacts.max(1);
        for i in 0..n {
            let st = stamp_for(i, n, self.tier.months, rng);
            let (p1, s1) = person(rng.below(ncontacts), rng);
            let (_p2, s2) = person(rng.below(ncontacts), rng);
            let dur = 15 + rng.below(75);
            let lang = LANGUAGES[i % LANGUAGES.len()];
            let id = format!("{}-call-{}", st.date(), i);
            let rel = format!("sources/transcripts/{}/{}/{}", st.year(), st.month(), id);
            let a1 = format!("records/contacts/{s1}");
            let a2 = format!("records/contacts/{s2}");
            let summary = format!("{} — {} call ({} min)", st.rfc3339(), p1, dur);
            let body = format!(
                "# Call transcript {date}\n\nAttendees: [[{a1}]], [[{a2}]]. \
                 Duration {dur} min. Language {lang}.\n",
                date = st.date(),
                a1 = a1,
                a2 = a2,
                dur = dur,
                lang = lang
            );
            let doc = Doc {
                rel,
                type_: "transcript".into(),
                summary,
                created: st.rfc3339(),
                updated: st.rfc3339(),
                fields: vec![
                    ("recorded_at".into(), st.rfc3339()),
                    ("duration_min".into(), dur.to_string()),
                    ("language".into(), yaml_quote(lang)),
                ],
                tags: vec!["call".into()],
                links: vec![a1.clone(), a2.clone()],
                body,
                link_lists: vec![("attendees".into(), vec![a1, a2])],
            };
            self.emit("sources/transcripts", doc);
        }
    }

    // ── sources/docs — date-sharded pdf-source type ──────────────────────────

    fn gen_docs(&mut self, rng: &mut Rng) {
        let n = self.tier.docs;
        let ncomp = self.tier.companies.max(1);
        for i in 0..n {
            let st = stamp_for(i, n, self.tier.months, rng);
            let (cdisp, cslug, _d) = company(rng.below(ncomp));
            let doc_type = DOC_TYPES[i % DOC_TYPES.len()];
            let id = format!("{}-{}-{}", st.date(), doc_type, i);
            let rel = format!("sources/docs/{}/{}/{}", st.year(), st.month(), id);
            let comp_link = format!("records/companies/{cslug}");
            let summary = format!("{doc_type} from {cdisp}");
            let body = format!(
                "# {doc_type} — {cdisp}\n\nReceived from [[{comp_link}]] on \
                 {date}. Document type: {doc_type}.\n",
                doc_type = doc_type,
                cdisp = cdisp,
                comp_link = comp_link,
                date = st.date()
            );
            let doc = Doc {
                rel,
                type_: "pdf-source".into(),
                summary,
                created: st.rfc3339(),
                updated: st.rfc3339(),
                fields: vec![
                    ("received_from".into(), yaml_quote(&cdisp)),
                    ("received_at".into(), st.rfc3339()),
                    ("doc_type".into(), yaml_quote(doc_type)),
                ],
                tags: vec![doc_type.to_string()],
                links: vec![comp_link],
                body,
                link_lists: vec![],
            };
            self.emit("sources/docs", doc);
        }
    }

    // ── records/expenses — date-sharded EVENT type ───────────────────────────

    fn gen_expenses(&mut self, rng: &mut Rng) {
        let n = self.tier.expenses;
        let ncomp = self.tier.companies.max(1);
        for i in 0..n {
            let st = stamp_for(i, n, self.tier.months, rng);
            let (cdisp, cslug, _d) = company(rng.below(ncomp));
            let amount = 50 + rng.below(9950);
            let cents = rng.below(100);
            let cat = EXPENSE_CATEGORIES[i % EXPENSE_CATEGORIES.len()];
            let vendor_link = format!("records/companies/{cslug}");
            let id = format!("{}-exp-{}", st.date(), i);
            let rel = format!("records/expenses/{}/{}/{}", st.year(), st.month(), id);
            let amount_str = format!("{amount}.{cents:02}");
            let summary = format!("{} — {amount_str} USD — {cdisp}", st.date());
            let body = format!(
                "# Expense {date}\n\n{amount_str} USD for {cat}, vendor \
                 [[{vendor_link}]].\n",
                date = st.date(),
                amount_str = amount_str,
                cat = cat,
                vendor_link = vendor_link
            );
            let doc = Doc {
                rel,
                type_: "expense".into(),
                summary,
                created: st.rfc3339(),
                updated: st.rfc3339(),
                fields: vec![
                    ("date".into(), st.date()),
                    ("amount".into(), amount_str.clone()),
                    ("currency".into(), "USD".into()),
                    ("category".into(), yaml_quote(cat)),
                    ("vendor".into(), format!("[[{vendor_link}]]")),
                ],
                tags: vec![cat.to_string()],
                links: vec![vendor_link],
                body,
                link_lists: vec![],
            };
            self.emit("records/expenses", doc);
        }
    }

    // ── records/invoices — date-sharded EVENT type ───────────────────────────

    fn gen_invoices(&mut self, rng: &mut Rng) {
        let n = self.tier.invoices;
        let ncomp = self.tier.companies.max(1);
        for i in 0..n {
            let st = stamp_for(i, n, self.tier.months, rng);
            let (cdisp, cslug, _d) = company(rng.below(ncomp));
            let amount = 500 + rng.below(49500);
            let status = INVOICE_STATUS[i % INVOICE_STATUS.len()];
            let vendor_link = format!("records/companies/{cslug}");
            let id = format!("{}-inv-{}", st.date(), i);
            let rel = format!("records/invoices/{}/{}/{}", st.year(), st.month(), id);
            let amount_str = format!("{amount}.00");
            let summary = format!("{cdisp} — {amount_str} — {status}");
            let mut fields = vec![
                ("date".into(), st.date()),
                ("amount".into(), amount_str.clone()),
                ("vendor".into(), format!("[[{vendor_link}]]")),
                ("status".into(), status.to_string()),
            ];
            if status == "paid" {
                fields.push(("paid_at".into(), st.date()));
            }
            let body = format!(
                "# Invoice {date}\n\n{amount_str} from [[{vendor_link}]], \
                 status {status}.\n",
                date = st.date(),
                amount_str = amount_str,
                vendor_link = vendor_link,
                status = status
            );
            let doc = Doc {
                rel,
                type_: "invoice".into(),
                summary,
                created: st.rfc3339(),
                updated: st.rfc3339(),
                fields,
                tags: vec![status.to_string()],
                links: vec![vendor_link],
                body,
                link_lists: vec![],
            };
            self.emit("records/invoices", doc);
        }
    }

    // ── records/meetings — date-sharded EVENT type, attendee link-list ───────

    fn gen_meetings(&mut self, rng: &mut Rng) {
        let n = self.tier.meetings;
        let ncontacts = self.tier.contacts.max(1);
        for i in 0..n {
            let st = stamp_for(i, n, self.tier.months, rng);
            let na = 2 + rng.below(3);
            let mut attendees = Vec::new();
            let mut names = Vec::new();
            for _ in 0..na {
                let (disp, slug) = person(rng.below(ncontacts), rng);
                attendees.push(format!("records/contacts/{slug}"));
                names.push(disp.split(' ').next().unwrap_or("").to_string());
            }
            let dur = 30 * (1 + rng.below(4));
            let id = format!("{}-meeting-{}", st.date(), i);
            let rel = format!("records/meetings/{}/{}/{}", st.year(), st.month(), id);
            let shown: Vec<String> = names.iter().take(3).cloned().collect();
            let extra = na.saturating_sub(3);
            let summary = if extra > 0 {
                format!("{} — {} (+{} more)", st.date(), shown.join(", "), extra)
            } else {
                format!("{} — {}", st.date(), shown.join(", "))
            };
            let body = format!(
                "# Meeting {date}\n\nAttendees: {}. Duration {dur} min.\n",
                attendees
                    .iter()
                    .map(|a| format!("[[{a}]]"))
                    .collect::<Vec<_>>()
                    .join(", "),
                date = st.date(),
                dur = dur
            );
            let doc = Doc {
                rel,
                type_: "meeting".into(),
                summary,
                created: st.rfc3339(),
                updated: st.rfc3339(),
                fields: vec![
                    ("date".into(), st.date()),
                    ("location".into(), yaml_quote("video")),
                    ("duration_min".into(), dur.to_string()),
                ],
                tags: vec!["meeting".into()],
                links: attendees.clone(),
                body,
                link_lists: vec![("attendees".into(), attendees)],
            };
            self.emit("records/meetings", doc);
        }
    }

    // ── wiki/people — FLAT, curation-bounded ─────────────────────────────────

    fn gen_wiki_people(&mut self, rng: &mut Rng) {
        let n = self.tier.wiki_people;
        let ncontacts = self.tier.contacts.max(1);
        for i in 0..n {
            let (disp, slug) = person(i, rng);
            let st = stamp_for(i, n, self.tier.months, rng);
            // wiki page synthesises from the matching contact record (if any)
            let cidx = i % ncontacts;
            let (_cd, cslug) = person(cidx, rng);
            let contact_link = format!("records/contacts/{cslug}");
            let summary = format!("Bio and relationship history for {disp}");
            let body = format!(
                "# {disp}\n\nSynthesis page for {disp}. Atomic record: \
                 [[{contact_link}]]. Background, decision style, and the \
                 relationship timeline live here.\n",
                disp = disp,
                contact_link = contact_link
            );
            let doc = Doc {
                rel: format!("wiki/people/{slug}"),
                type_: "wiki-page".into(),
                summary,
                created: st.rfc3339(),
                updated: st.rfc3339(),
                fields: vec![
                    ("topic".into(), yaml_quote(&disp)),
                    ("derived_from".into(), format!("[[{contact_link}]]")),
                ],
                tags: vec!["person".into()],
                links: vec![contact_link],
                body,
                link_lists: vec![],
            };
            self.emit("wiki/people", doc);
        }
    }

    // ── wiki/projects — FLAT, curation-bounded ───────────────────────────────

    fn gen_wiki_projects(&mut self, rng: &mut Rng) {
        let n = self.tier.wiki_projects;
        let ncontacts = self.tier.contacts.max(1);
        for i in 0..n {
            let base = PROJECT_NAMES[i % PROJECT_NAMES.len()];
            let slug = format!("{base}-{i}");
            let st = stamp_for(i, n, self.tier.months, rng);
            let (lead_disp, lead_slug) = person(rng.below(ncontacts), rng);
            let lead_link = format!("records/contacts/{lead_slug}");
            let summary = format!("Project {base}; lead {lead_disp}");
            let body = format!(
                "# {base}\n\nProject synthesis. Lead: [[{lead_link}]]. Status, \
                 milestones, and linked decisions tracked here.\n",
                base = base,
                lead_link = lead_link
            );
            let doc = Doc {
                rel: format!("wiki/projects/{slug}"),
                type_: "wiki-page".into(),
                summary,
                created: st.rfc3339(),
                updated: st.rfc3339(),
                fields: vec![
                    ("topic".into(), yaml_quote(base)),
                    ("derived_from".into(), format!("[[{lead_link}]]")),
                ],
                tags: vec!["project".into()],
                links: vec![lead_link],
                body,
                link_lists: vec![],
            };
            self.emit("wiki/projects", doc);
        }
    }

    // ── index.jsonl — one complete twin per type-folder (UNCAPPED) ───────────

    fn flush_jsonl(&self) -> io::Result<()> {
        for (folder, body) in &self.jsonl {
            let path = self.out.join(folder).join("index.jsonl");
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, body)?;
        }
        Ok(())
    }

    // ── index.md hierarchy: type-folder (capped 500) + layer + root ──────────

    fn write_all_indexes(&mut self) -> io::Result<()> {
        const CAP: usize = 500;
        // Type-folder indexes. Sort each by recency (updated desc, path asc) and
        // cap at 500; overflow gets a `## More` footer. The jsonl twin (already
        // written) stays complete — this is the cap-vs-complete invariant.
        // Collect per-layer rollups while we go.
        let mut layer_folders: BTreeMap<&str, Vec<(String, usize)>> = BTreeMap::new();
        layer_folders.insert("sources", vec![]);
        layer_folders.insert("records", vec![]);
        layer_folders.insert("wiki", vec![]);

        // We must iterate in a stable order; BTreeMap already is sorted by key.
        let folders: Vec<String> = self.indexes.keys().cloned().collect();
        for folder in folders {
            let entries = self.indexes.get_mut(&folder).unwrap();
            let total = entries.len();
            // recency sort: updated DESC, then path ASC (total order)
            entries.sort_by(|a, b| {
                b.updated
                    .cmp(&a.updated)
                    .then_with(|| a.rel.cmp(&b.rel))
            });
            let layer = folder.split('/').next().unwrap_or("");
            let short = folder.splitn(2, '/').nth(1).unwrap_or(&folder).to_string();
            layer_folders
                .get_mut(layer)
                .map(|v| v.push((folder.clone(), total)));

            let path = self.out.join(&folder).join("index.md");
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut s = String::new();
            s.push_str("---\n");
            s.push_str("type: index\n");
            s.push_str("scope: type-folder\n");
            s.push_str(&format!("folder: {}\n", folder));
            s.push_str(&format!("updated: {}\n", anchor_now()));
            s.push_str("---\n\n");
            s.push_str(&format!("# {}\n\n", folder));
            let shown = total.min(CAP);
            for e in entries.iter().take(shown) {
                s.push_str(&index_line(e));
            }
            if total > CAP {
                let layer_word = layer;
                let type_word = type_from_folder(&short);
                s.push_str(&format!(
                    "\n## More\n\nThis folder has {total} files. The {CAP} most recent are listed above.\nUse `dbmd index query --type {type_word} --in {layer_word}` for the complete catalog.\n",
                    total = total,
                    CAP = CAP,
                    type_word = type_word,
                    layer_word = layer_word
                ));
            }
            fs::write(path, s)?;
        }

        // Layer indexes.
        for (layer, folders) in &layer_folders {
            if folders.is_empty() {
                continue;
            }
            let path = self.out.join(layer).join("index.md");
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut s = String::new();
            s.push_str("---\n");
            s.push_str("type: index\n");
            s.push_str("scope: layer\n");
            s.push_str(&format!("folder: {}\n", layer));
            s.push_str(&format!("updated: {}\n", anchor_now()));
            s.push_str("---\n\n");
            s.push_str(&format!("# {}\n\n", layer));
            // stable order by folder name
            let mut sorted = folders.clone();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            for (folder, count) in &sorted {
                let short = folder.splitn(2, '/').nth(1).unwrap_or(folder);
                let disp = title_case(short);
                s.push_str(&format!(
                    "- [[{folder}/index|{disp}]] ({count} files)\n",
                    folder = folder,
                    disp = disp,
                    count = count
                ));
            }
            fs::write(path, s)?;
        }

        // Root index.
        let mut s = String::new();
        s.push_str("---\n");
        s.push_str("type: index\n");
        s.push_str("scope: root\n");
        s.push_str(&format!("updated: {}\n", anchor_now()));
        s.push_str("---\n\n");
        s.push_str("# Knowledge base index\n\n");
        for (layer, title) in [
            ("sources", "Sources"),
            ("records", "Records"),
            ("wiki", "Wiki"),
        ] {
            let folders = &layer_folders[layer];
            if folders.is_empty() {
                continue;
            }
            s.push_str(&format!("## {}\n", title));
            let mut sorted = folders.clone();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            for (folder, count) in &sorted {
                let short = folder.splitn(2, '/').nth(1).unwrap_or(folder);
                let disp = title_case(short);
                s.push_str(&format!(
                    "- [[{folder}/index|{disp}]] ({count} files)\n",
                    folder = folder,
                    disp = disp,
                    count = count
                ));
            }
            s.push('\n');
        }
        fs::write(self.out.join("index.md"), s)?;
        Ok(())
    }

    // ── log.md (active month) + log/<YYYY-MM>.md (archives) ──────────────────

    fn write_log(&mut self) -> io::Result<()> {
        // Ensure there is at least one entry per month so rotation is real and
        // every archive parses. We already scattered ingest entries during email
        // generation; add a deterministic create/update/validate spine here.
        for back in (0..self.tier.months).rev() {
            let (y, mo) = month_back(back);
            let dim = days_in_month(y, mo);
            let st = Stamp {
                y,
                mo,
                d: dim.min(15),
                h: 9,
                mi: 0,
            };
            self.log(
                &st,
                "create",
                "records/contacts/index",
                "Monthly contact reconciliation.",
            );
            let st2 = Stamp { mi: 30, ..st };
            self.log(
                &st2,
                "index-rebuild",
                "",
                "Full hierarchy rebuilt after monthly bulk drop.",
            );
            let st3 = Stamp {
                d: dim,
                mi: 45,
                ..st
            };
            self.log(
                &st3,
                "validate",
                "",
                "PASS — 0 errors (monthly sweep).",
            );
        }

        // The anchor month is the ACTIVE log.md; everything older rotates.
        let anchor_key = format!("{:04}-{:02}", ANCHOR_YEAR, ANCHOR_MONTH);

        // Active log.md
        let active_body = self
            .log_by_month
            .get(&anchor_key)
            .cloned()
            .unwrap_or_default();
        let mut active = String::new();
        active.push_str("---\ntype: log\n---\n\n# Curator log\n\n");
        active.push_str(&active_body);
        fs::write(self.out.join("log.md"), active)?;

        // Archives: every month that is NOT the anchor → log/<YYYY-MM>.md
        let log_dir = self.out.join("log");
        let mut wrote_archive = false;
        for (ym, body) in &self.log_by_month {
            if *ym == anchor_key {
                continue;
            }
            if !wrote_archive {
                fs::create_dir_all(&log_dir)?;
                wrote_archive = true;
            }
            let mut a = String::new();
            a.push_str(&format!("---\ntype: log\narchive: {ym}\n---\n\n"));
            a.push_str(&format!("# Curator log — {ym}\n\n"));
            a.push_str(body);
            fs::write(log_dir.join(format!("{ym}.md")), a)?;
        }
        Ok(())
    }

    /// `db/.gitattributes`-style floor: union-merge the active log so concurrent
    /// clones never lose an entry (SPEC § log.md concurrent-clone merges).
    fn write_gitattributes(&self) -> io::Result<()> {
        fs::write(
            self.out.join(".gitattributes"),
            "log.md merge=union\nlog/*.md merge=union\n",
        )
    }
}

// ─────────────────────────── small format helpers ─────────────────────────

/// The anchor "now" as RFC3339 UTC — the `updated` stamp on generated indexes.
fn anchor_now() -> String {
    format!("{:04}-{:02}-28T00:00:00Z", ANCHOR_YEAR, ANCHOR_MONTH)
}

/// One `index.md` catalog line: `- [[<rel>]] — <summary>  ·  #tag #tag`.
fn index_line(e: &IndexEntry) -> String {
    let mut line = format!("- [[{}]] — {}", e.rel, e.summary);
    if !e.tags.is_empty() {
        line.push_str("  ·  ");
        line.push_str(
            &e.tags
                .iter()
                .map(|t| format!("#{t}"))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    line.push('\n');
    line
}

/// Map a type-folder short name back to its singular `type` for the `## More`
/// footer's `dbmd index query --type <t>` hint (e.g. `emails` → `email`).
fn type_from_folder(short: &str) -> &str {
    match short {
        "emails" => "email",
        "transcripts" => "transcript",
        "docs" => "pdf-source",
        "contacts" => "contact",
        "companies" => "company",
        "expenses" => "expense",
        "invoices" => "invoice",
        "meetings" => "meeting",
        "people" | "projects" => "wiki-page",
        other => other,
    }
}

/// Title-case a folder short name for display in layer/root index links.
fn title_case(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Slugify an email subject for use in ids / tags.
fn subject_slug(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}
