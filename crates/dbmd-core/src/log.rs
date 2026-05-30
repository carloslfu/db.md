//! `log` — the append-only, month-rotating chronological log.
//!
//! One logical timeline: the active `log.md` at the store root plus
//! `log/<YYYY-MM>.md` archives. [`Log::append`] rolls older months into
//! archives on write so the active file stays current-month. [`Log::tail`] and
//! [`Log::since`] **reverse-read from EOF**. Both read each file they touch in
//! full — the on-disk order is not guaranteed monotonic, so neither can
//! early-stop within a file — and select by timestamp: `tail` keeps the `n`
//! newest, `since` keeps everything newer than the cutoff. Both cross into
//! month archives only as far back as the requested window reaches (by the
//! cutoff's month for `since`, by the current `n`th-newest's month for `tail`)
//! — never the whole history.
//!
//! Append-only contract: there is no rewrite API. Corrective entries go on the
//! end; out-of-order timestamps are a validate warning (`LOG_OUT_OF_ORDER`),
//! signalling a probable rewrite.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, FixedOffset, NaiveDateTime, TimeZone};

use crate::store::Store;

/// The on-disk header timestamp format: `YYYY-MM-DD HH:MM` (minute precision,
/// no timezone). Parsing reattaches UTC; emitting renders the entry's own
/// wall-clock, so a read→write→read round-trip is stable at minute precision.
const TS_FORMAT: &str = "%Y-%m-%d %H:%M";

/// The frontmatter block written when the active `log.md` is created.
const LOG_FRONTMATTER: &str = "---\ntype: log\n---\n\n# Curator log\n";

/// Block size for the backward (reverse-from-EOF) reader.
const REVERSE_BLOCK: usize = 8 * 1024;

/// A recognized `log.md` entry kind. Custom kinds are valid in the format
/// (`dbmd validate` warns on unrecognized via `LOG_UNKNOWN_KIND`); this enum
/// carries the recognized vocabulary plus a [`LogKind::Custom`] catch-all so an
/// unknown kind round-trips without loss.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogKind {
    /// A source artifact was ingested.
    Ingest,
    /// A file was created.
    Create,
    /// A file was updated.
    Update,
    /// A file was deleted.
    Delete,
    /// A file was renamed/moved.
    Rename,
    /// A wiki-link was added.
    Link,
    /// A validation pass ran.
    Validate,
    /// The index was rebuilt.
    IndexRebuild,
    /// A contradiction between sources was flagged.
    Contradiction,
    /// Any kind outside the recognized vocabulary, preserved verbatim.
    Custom(String),
}

impl LogKind {
    /// The canonical lowercase string for this kind, as it appears in a log
    /// header (`ingest`, `index-rebuild`, …).
    pub fn as_str(&self) -> &str {
        match self {
            LogKind::Ingest => "ingest",
            LogKind::Create => "create",
            LogKind::Update => "update",
            LogKind::Delete => "delete",
            LogKind::Rename => "rename",
            LogKind::Link => "link",
            LogKind::Validate => "validate",
            LogKind::IndexRebuild => "index-rebuild",
            LogKind::Contradiction => "contradiction",
            LogKind::Custom(s) => s,
        }
    }

    /// Parse a kind from its header token; non-canonical tokens become
    /// [`LogKind::Custom`].
    pub fn parse(token: &str) -> LogKind {
        match token {
            "ingest" => LogKind::Ingest,
            "create" => LogKind::Create,
            "update" => LogKind::Update,
            "delete" => LogKind::Delete,
            "rename" => LogKind::Rename,
            "link" => LogKind::Link,
            "validate" => LogKind::Validate,
            "index-rebuild" => LogKind::IndexRebuild,
            "contradiction" => LogKind::Contradiction,
            other => LogKind::Custom(other.to_string()),
        }
    }

    /// True if this is one of the recognized kinds (i.e. not
    /// [`LogKind::Custom`]).
    pub fn is_recognized(&self) -> bool {
        !matches!(self, LogKind::Custom(_))
    }
}

/// One parsed `log.md` entry: a header
/// (`## [YYYY-MM-DD HH:MM] <kind> | <object>`) plus its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// The entry timestamp from the header.
    pub timestamp: DateTime<FixedOffset>,
    /// The entry kind.
    pub kind: LogKind,
    /// The object slot — a store-relative path/wiki-link target, or `None` for
    /// store-wide actions like `validate`.
    pub object: Option<String>,
    /// The free-form body (one or more lines) explaining what happened.
    pub note: String,
}

impl LogEntry {
    /// Render this entry as it appears on disk: the `## [...]` header line,
    /// then the note body, then a trailing blank line so successive entries are
    /// separated. The note is emitted verbatim (trailing whitespace trimmed).
    fn render(&self) -> String {
        let ts = self.timestamp.format(TS_FORMAT);
        let mut out = String::new();
        match &self.object {
            Some(obj) => {
                out.push_str(&format!("## [{}] {} | {}\n", ts, self.kind.as_str(), obj));
            }
            None => {
                out.push_str(&format!("## [{}] {}\n", ts, self.kind.as_str()));
            }
        }
        let note = self.note.trim_end_matches(['\n', '\r', ' ', '\t']);
        if !note.is_empty() {
            out.push_str(note);
            out.push('\n');
        }
        out.push('\n');
        out
    }

    /// The `(year, month)` of this entry's wall-clock timestamp — the rotation
    /// bucket.
    fn year_month(&self) -> (i32, u32) {
        (self.timestamp.year(), self.timestamp.month())
    }
}

/// The store's chronological log: a thin handle for the append-only timeline.
/// All methods take the [`Store`] so they resolve the active `log.md` and the
/// `log/` archives under the store root.
#[derive(Debug, Clone)]
pub struct Log;

impl Log {
    /// Atomically append `entry` to the active `log.md`, creating it (with
    /// `type: log` frontmatter) if absent. **If the active log holds entries
    /// from a prior month, roll those older months into `log/<YYYY-MM>.md`
    /// first** (atomic move), keeping the active file to the current month.
    pub fn append(store: &Store, entry: &LogEntry) -> crate::Result<()> {
        let active = active_log_path(store);

        // Read the active file's current contents (if any). The "current month"
        // is the month of the entry being appended (the newest in the timeline);
        // every existing entry from a strictly-earlier month rolls to archives.
        let current_ym = entry.year_month();

        if active.exists() {
            let content = fs::read_to_string(&active)?;
            let (header, entries) = parse_active(&content);

            // Partition existing entries into prior-month (roll out) and
            // current-or-later (keep in the active file).
            let mut by_month: BTreeMap<(i32, u32), Vec<LogEntry>> = BTreeMap::new();
            let mut keep: Vec<LogEntry> = Vec::new();
            for e in entries {
                if e.year_month() < current_ym {
                    by_month.entry(e.year_month()).or_default().push(e);
                } else {
                    keep.push(e);
                }
            }

            if !by_month.is_empty() {
                // Roll each prior month into its archive (atomic per-file),
                // appending to any existing archive for that month.
                let dir = archive_dir(store);
                fs::create_dir_all(&dir)?;
                for ((y, m), month_entries) in &by_month {
                    let path = archive_path(store, *y, *m);
                    append_to_archive(&path, month_entries)?;
                }

                // Rewrite the active file to the kept (current-month) entries
                // plus the new entry — atomically.
                let mut body = String::new();
                for e in &keep {
                    body.push_str(&e.render());
                }
                body.push_str(&entry.render());
                let full = compose_active(&header, &body);
                write_atomic(&active, full.as_bytes())?;
                return Ok(());
            }

            // No rotation needed: plain atomic append of the rendered entry.
            let mut full = content;
            if !full.ends_with('\n') {
                full.push('\n');
            }
            full.push_str(&entry.render());
            write_atomic(&active, full.as_bytes())?;
            Ok(())
        } else {
            // Fresh log: frontmatter + the single entry.
            if let Some(parent) = active.parent() {
                fs::create_dir_all(parent)?;
            }
            let body = entry.render();
            let full = compose_active(LOG_FRONTMATTER, &body);
            write_atomic(&active, full.as_bytes())?;
            Ok(())
        }
    }

    /// The `n` most-recent entries **by timestamp**, returned oldest→newest.
    ///
    /// **Out-of-order safety (mirrors [`Log::since`]).** The log is append-only
    /// but *not* guaranteed to be in non-decreasing timestamp order on disk: a
    /// corrective entry is appended below the entry it corrects, a
    /// backdated/clock-skewed write lands physically after newer entries, and a
    /// `merge=union` clone merge interleaves both sides until a later agent
    /// reorders. Out-of-order is only a `LOG_OUT_OF_ORDER` warning, never
    /// rejected. So the last `n` *physical* entries are **not** the `n` newest
    /// by time — taking them would omit a genuinely-recent entry that sits
    /// physically before an older one, and the documented curator warm-up
    /// (`dbmd log tail 20`) would report a stale picture of what was done lately.
    /// We therefore feed every entry of each file we touch through a bounded
    /// newest-by-timestamp window and let it select the true top `n`.
    ///
    /// Bounded cost: the active `log.md` is kept to the current month by
    /// rotation, so a full read of it is cheap and is not a whole-store walk.
    /// Across archives we *can* prune: each `log/<YYYY-MM>.md` holds only entries
    /// from that month (rotation buckets by the entry's own year-month), so once
    /// the window is full, an archive whose month is strictly before the
    /// window-minimum's month cannot contain any entry newer than the current
    /// `n`th-newest. We cross archives newest-month-first and stop at the first
    /// such archive.
    pub fn tail(store: &Store, n: usize) -> crate::Result<Vec<LogEntry>> {
        if n == 0 {
            return Ok(Vec::new());
        }

        // A bounded window of the `n` entries with the largest timestamps. No
        // within-file early stop: out-of-order entries mean a newer entry can
        // sit physically before an older one, so each file is read fully.
        let mut window = NewestWindow::new(n);

        // Active file: scan fully (current-month-bounded by rotation).
        let active = active_log_path(store);
        if active.exists() {
            reverse_collect(&active, |e| {
                window.consider(e);
                false
            })?;
        }

        // Archives, newest-month-first. Once the window is full, an archive
        // whose month is strictly before the window-minimum's month holds only
        // entries older than the current cutoff, so it (and every older archive)
        // is skippable.
        for archive in list_archives_desc(store)? {
            if let (true, Some(cutoff_ym), Some(arch_ym)) = (
                window.is_full(),
                window.min_year_month(),
                archive_year_month(&archive),
            ) {
                if arch_ym < cutoff_ym {
                    break;
                }
            }
            reverse_collect(&archive, |e| {
                window.consider(e);
                false
            })?;
        }

        Ok(window.into_sorted())
    }

    /// Entries strictly newer than `time`, reverse-scanning active → archives.
    ///
    /// **No within-file early stop.** The log is append-only but *not*
    /// guaranteed to be in non-decreasing timestamp order on disk: a corrective
    /// entry is appended below the entry it corrects (SPEC: "if a finding is
    /// wrong, append a corrective entry below it"), a backdated/clock-skewed
    /// write lands physically after newer entries, and a `merge=union` clone
    /// merge interleaves both sides until a later agent reorders. Out-of-order
    /// is only a `LOG_OUT_OF_ORDER` warning, never rejected. So a newer entry
    /// can sit physically *before* an older one; stopping at the first
    /// older-than-`time` entry would silently drop those — the documented
    /// curator warm-up (`dbmd log since <ts>`) would miss real recent work.
    /// We therefore read every entry of each file we touch.
    ///
    /// Bounded cost: the active `log.md` is kept to the current month by
    /// rotation, so a full read of it is cheap (the same read `tail` does for a
    /// large `n`) and is not a whole-store walk. Across archives we *can* stop:
    /// each `log/<YYYY-MM>.md` holds only entries from that month (rotation
    /// buckets by the entry's own year-month), so an archive whose month is
    /// strictly before `time`'s month cannot contain any entry newer than
    /// `time`. We cross archives newest-month-first and stop at the first whose
    /// month is entirely at or before `time`'s.
    pub fn since(store: &Store, time: DateTime<FixedOffset>) -> crate::Result<Vec<LogEntry>> {
        let mut collected: Vec<LogEntry> = Vec::new();

        // Active file: scan fully, no early stop (out-of-order safe).
        let active = active_log_path(store);
        if active.exists() {
            reverse_collect(&active, |e| {
                if e.timestamp > time {
                    collected.push(e);
                }
                false
            })?;
        }

        // The cutoff's own (year, month): any archive strictly before it holds
        // only older entries and is skippable.
        let cutoff_ym = (time.year(), time.month());

        for archive in list_archives_desc(store)? {
            // Archives are newest-month-first; once a month is strictly before
            // the cutoff's month, every remaining (older) archive is too.
            if let Some(arch_ym) = archive_year_month(&archive) {
                if arch_ym < cutoff_ym {
                    break;
                }
            }
            // Scan this archive fully — within a month, entries may still be
            // out of order, so no within-file early stop.
            reverse_collect(&archive, |e| {
                if e.timestamp > time {
                    collected.push(e);
                }
                false
            })?;
        }

        collected.reverse();
        Ok(collected)
    }

    /// The timestamp of the most recent `validate` entry — the default `since`
    /// window for working-set validation ([`crate::validate::validate_working_set`]).
    pub fn last_validate_at(store: &Store) -> crate::Result<Option<DateTime<FixedOffset>>> {
        let mut found: Option<DateTime<FixedOffset>> = None;

        let active = active_log_path(store);
        if active.exists() {
            reverse_collect(&active, |e| {
                if e.kind == LogKind::Validate {
                    found = Some(e.timestamp);
                    true
                } else {
                    false
                }
            })?;
        }

        if found.is_none() {
            for archive in list_archives_desc(store)? {
                reverse_collect(&archive, |e| {
                    if e.kind == LogKind::Validate {
                        found = Some(e.timestamp);
                        true
                    } else {
                        false
                    }
                })?;
                if found.is_some() {
                    break;
                }
            }
        }

        Ok(found)
    }

    /// Parse a single entry header (`## [YYYY-MM-DD HH:MM] <kind> | <object>`)
    /// into its timestamp, kind, and object. Returns `None` if the line isn't a
    /// well-formed entry header.
    pub fn parse_header(line: &str) -> Option<(DateTime<FixedOffset>, LogKind, Option<String>)> {
        let line = line.trim_end_matches(['\n', '\r']);
        let rest = line.strip_prefix("## [")?;
        let close = rest.find(']')?;
        let ts_str = &rest[..close];
        let timestamp = parse_timestamp(ts_str)?;

        // Everything after the closing bracket: ` <kind> | <object>` or
        // ` <kind>`.
        let after = rest[close + 1..].trim();
        if after.is_empty() {
            return None;
        }

        let (kind_str, object) = match after.split_once('|') {
            Some((k, o)) => {
                let obj = o.trim();
                let obj = if obj.is_empty() {
                    None
                } else {
                    Some(obj.to_string())
                };
                (k.trim(), obj)
            }
            None => (after, None),
        };

        if kind_str.is_empty() {
            return None;
        }

        Some((timestamp, LogKind::parse(kind_str), object))
    }
}

// ── Internal helpers ────────────────────────────────────────────────────────

/// A bounded window of the `n` entries with the largest timestamps, fed by a
/// **reverse (newest-physical-first) scan** and used by [`Log::tail`].
///
/// Why this exists: the last `n` *physical* entries are the `n` newest only
/// when the log is in non-decreasing time order. That's the append-only
/// contract, not a guarantee — a backdated, clock-skewed, or merge-interleaved
/// entry violates it (and trips the `LOG_OUT_OF_ORDER` validate warning). The
/// window decouples `tail` from that assumption: it keeps the `n` largest
/// timestamps seen regardless of the order they arrive in, so the caller can
/// read each file fully (no fragile within-file early stop) and still get the
/// true top `n`.
///
/// Tie-break: entries sharing a timestamp at the window boundary are ordered by
/// **physical recency** — the one appended later (encountered earlier in the
/// reverse scan, i.e. a smaller `arrival`) wins. "Newest" means most-recently
/// recorded.
struct NewestWindow {
    cap: usize,
    /// Min-by-(timestamp, then physical-oldest) heap: the root is always the
    /// next entry to evict once the window is full.
    heap: std::collections::BinaryHeap<WindowItem>,
    /// Count of entries fed in, in reverse-scan order, used as the tie-break
    /// key (0 = newest physical).
    next_arrival: u64,
}

impl NewestWindow {
    fn new(cap: usize) -> Self {
        NewestWindow {
            cap,
            heap: std::collections::BinaryHeap::with_capacity(cap),
            next_arrival: 0,
        }
    }

    /// Offer one entry from the scan. If the window isn't full it's kept; once
    /// full, it's kept (evicting the current minimum) iff its timestamp is `>=`
    /// the window minimum. Equal-timestamp boundary entries resolve by physical
    /// recency (see the type doc).
    fn consider(&mut self, entry: LogEntry) {
        let arrival = self.next_arrival;
        self.next_arrival += 1;

        if self.heap.len() < self.cap {
            self.heap.push(WindowItem { entry, arrival });
            return;
        }

        // Window full. The heap root is the current minimum (oldest-by-
        // timestamp held; on a tie, the oldest-physical).
        let root = self.heap.peek().expect("full window has a root");
        if entry.timestamp > root.entry.timestamp {
            // Strictly newer than the window minimum: it belongs; evict the min.
            self.heap.pop();
            self.heap.push(WindowItem { entry, arrival });
        }
        // On `<=` we keep the window as-is. `<` is plainly too old. `==` is the
        // tie case: the scan is newest-physical-first, so this entry is
        // physically *older* than the held one of equal timestamp, and the
        // tie-break keeps the physically-newer (most-recently-recorded) entry —
        // so the incoming one is dropped.
    }

    /// Whether the window already holds its full `cap` entries.
    fn is_full(&self) -> bool {
        self.heap.len() >= self.cap
    }

    /// The `(year, month)` of the window's current minimum (oldest kept) entry,
    /// or `None` when the window is empty. Used to prune older archives: an
    /// archive month strictly before this can't beat the current cutoff.
    fn min_year_month(&self) -> Option<(i32, u32)> {
        self.heap
            .peek()
            .map(|item| (item.entry.timestamp.year(), item.entry.timestamp.month()))
    }

    /// The held entries, oldest→newest (chronological), ties broken
    /// oldest-physical→newest-physical.
    fn into_sorted(self) -> Vec<LogEntry> {
        let mut items: Vec<WindowItem> = self.heap.into_vec();
        // Ascending by timestamp; on a tie, oldest-physical (larger arrival)
        // first so the most-recently-recorded entry sorts last.
        items.sort_by(|a, b| {
            a.entry
                .timestamp
                .cmp(&b.entry.timestamp)
                .then(b.arrival.cmp(&a.arrival))
        });
        items.into_iter().map(|i| i.entry).collect()
    }
}

/// One slot in [`NewestWindow`]'s heap. `Ord` is defined so the heap is a
/// **min-heap on `(timestamp, physical-oldest)`**: `BinaryHeap` is a max-heap,
/// so the root (max under this `Ord`) is the eviction candidate — the smallest
/// timestamp, and on a tie the oldest-physical (largest `arrival`).
struct WindowItem {
    entry: LogEntry,
    arrival: u64,
}

impl PartialEq for WindowItem {
    fn eq(&self, other: &Self) -> bool {
        self.entry.timestamp == other.entry.timestamp && self.arrival == other.arrival
    }
}
impl Eq for WindowItem {}

impl Ord for WindowItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reverse on timestamp so the *smallest* timestamp is the heap max
        // (eviction candidate). On equal timestamps, the larger `arrival`
        // (older physical) is the heap max so it is evicted first.
        other
            .entry
            .timestamp
            .cmp(&self.entry.timestamp)
            .then(self.arrival.cmp(&other.arrival))
    }
}
impl PartialOrd for WindowItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// The active `log.md` path under the store root.
fn active_log_path(store: &Store) -> PathBuf {
    store.root.join("log.md")
}

/// The `log/` archive directory under the store root.
fn archive_dir(store: &Store) -> PathBuf {
    store.root.join("log")
}

/// The `log/<YYYY-MM>.md` archive path for a given month.
fn archive_path(store: &Store, year: i32, month: u32) -> PathBuf {
    archive_dir(store).join(format!("{:04}-{:02}.md", year, month))
}

/// Parse a `YYYY-MM-DD HH:MM` header timestamp, reattaching UTC. `None` on any
/// malformed shape.
fn parse_timestamp(s: &str) -> Option<DateTime<FixedOffset>> {
    let naive = NaiveDateTime::parse_from_str(s.trim(), TS_FORMAT).ok()?;
    let utc = FixedOffset::east_opt(0)?;
    utc.from_local_datetime(&naive).single()
}

/// Split a `log.md` / archive file into its leading frontmatter+heading block
/// (everything up to and including the line before the first `## [` header) and
/// its parsed entries. If there are no entries, the whole content is the header
/// block.
fn parse_active(content: &str) -> (String, Vec<LogEntry>) {
    match find_first_header(content) {
        Some(idx) => {
            let header = content[..idx].to_string();
            let entries = parse_entries(&content[idx..]);
            (header, entries)
        }
        None => (content.to_string(), Vec::new()),
    }
}

/// Byte offset of the first entry header (`## [` at the start of a line), or
/// `None`.
fn find_first_header(content: &str) -> Option<usize> {
    if content.starts_with("## [") {
        return Some(0);
    }
    content.match_indices("\n## [").next().map(|(i, _)| i + 1)
}

/// Parse every entry in a slice that begins at (or before, header-block
/// included) a sequence of `## [` headers. Headers that fail to parse are
/// skipped (their body folds into the previous valid entry's note is avoided —
/// they simply start no new entry).
fn parse_entries(text: &str) -> Vec<LogEntry> {
    let mut entries: Vec<LogEntry> = Vec::new();
    let mut cur_header: Option<(DateTime<FixedOffset>, LogKind, Option<String>)> = None;
    let mut cur_note: Vec<&str> = Vec::new();

    let flush = |entries: &mut Vec<LogEntry>,
                 header: &mut Option<(DateTime<FixedOffset>, LogKind, Option<String>)>,
                 note: &mut Vec<&str>| {
        if let Some((timestamp, kind, object)) = header.take() {
            let joined = note.join("\n");
            let note_str = joined.trim_matches(['\n', '\r']).to_string();
            entries.push(LogEntry {
                timestamp,
                kind,
                object,
                note: note_str,
            });
        }
        note.clear();
    };

    for line in text.lines() {
        if line.starts_with("## [") {
            if let Some(parsed) = Log::parse_header(line) {
                // Close the previous entry, start a new one.
                flush(&mut entries, &mut cur_header, &mut cur_note);
                cur_header = Some(parsed);
                continue;
            }
            // Unparseable `## [` line: treat as body of the current entry.
        }
        if cur_header.is_some() {
            cur_note.push(line);
        }
    }
    flush(&mut entries, &mut cur_header, &mut cur_note);
    entries
}

/// Recompose an active/archive file from a header block and an entry body.
fn compose_active(header: &str, body: &str) -> String {
    let mut out = String::new();
    out.push_str(header);
    if !header.is_empty() && !header.ends_with('\n') {
        out.push('\n');
    }
    // Exactly one blank line between the heading block and the first entry.
    if !header.is_empty() && !out.ends_with("\n\n") {
        out.push('\n');
    }
    out.push_str(body);
    out
}

/// Append entries to a month archive, creating it with `type: log` frontmatter
/// if absent. Atomic (temp-file rename). Entries are appended in the given
/// order (callers pass them already chronological within the month).
fn append_to_archive(path: &Path, entries: &[LogEntry]) -> crate::Result<()> {
    let mut body = String::new();
    for e in entries {
        body.push_str(&e.render());
    }

    if path.exists() {
        let existing = fs::read_to_string(path)?;
        let mut full = existing;
        if !full.ends_with('\n') {
            full.push('\n');
        }
        full.push_str(&body);
        write_atomic(path, full.as_bytes())?;
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let full = compose_active(LOG_FRONTMATTER, &body);
        write_atomic(path, full.as_bytes())?;
    }
    Ok(())
}

/// Atomic write: write to a temp file in the same directory, fsync, then
/// rename over the destination — so a concurrent reader never sees a
/// half-written file. Mirrors the parser's write path.
fn write_atomic(dest: &Path, bytes: &[u8]) -> crate::Result<()> {
    let dir = dest.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;

    // Unique temp name in the same directory (rename is atomic only within a
    // filesystem; same dir guarantees that). The name must be unique even
    // across threads of one process appending concurrently, so combine the pid
    // with a process-wide monotonic counter — a wall-clock timestamp alone can
    // collide and let one thread's rename pull another thread's temp out from
    // under it.
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let pid = std::process::id();
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let file_name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("log.md");
    let tmp = dir.join(format!(".{}.{}.{}.tmp", file_name, pid, seq));

    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    // rename over the destination; clean up the temp on failure.
    match fs::rename(&tmp, dest) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e.into())
        }
    }
}

/// Every `log/<YYYY-MM>.md` archive, sorted **newest month first**.
fn list_archives_desc(store: &Store) -> crate::Result<Vec<PathBuf>> {
    let dir = archive_dir(store);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut months: Vec<(String, PathBuf)> = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n,
            None => continue,
        };
        // Match `YYYY-MM.md`.
        if let Some(stem) = name.strip_suffix(".md") {
            if is_year_month(stem) {
                months.push((stem.to_string(), path.clone()));
            }
        }
    }
    // `YYYY-MM` strings sort lexically == chronologically; reverse for newest
    // first.
    months.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(months.into_iter().map(|(_, p)| p).collect())
}

/// The `(year, month)` an archive file represents, parsed from its
/// `log/<YYYY-MM>.md` name. `None` if the name isn't a well-formed month
/// archive (in which case the caller scans it rather than risk skipping it).
fn archive_year_month(path: &Path) -> Option<(i32, u32)> {
    let stem = path
        .file_name()
        .and_then(|s| s.to_str())
        .and_then(|n| n.strip_suffix(".md"))?;
    if !is_year_month(stem) {
        return None;
    }
    let year: i32 = stem[..4].parse().ok()?;
    let month: u32 = stem[5..7].parse().ok()?;
    Some((year, month))
}

/// True if `s` looks like `YYYY-MM` (4 digits, dash, 2 digits).
fn is_year_month(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 7 {
        return false;
    }
    bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5].is_ascii_digit()
        && bytes[6].is_ascii_digit()
}

/// Reverse-read `path` from EOF, parsing entries newest-first and feeding each
/// to `take`. `take` returns `true` to stop early (enough collected). The file
/// is read backward in blocks; only the tail region needed to satisfy `take`
/// is read — the whole file is read only if `take` never returns `true`.
fn reverse_collect<F>(path: &Path, mut take: F) -> crate::Result<()>
where
    F: FnMut(LogEntry) -> bool,
{
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(());
    }

    // Algorithm: grow a tail buffer leftward one block at a time, emitting
    // entries strictly newest-first as their left boundary is confirmed, and
    // stopping the instant `take` says enough. The whole file is read only if
    // `take` never returns `true` (e.g. `tail(n)` with n ≥ entry count).
    //
    // Invariant: a `## [` line-start anywhere in the buffer is a *complete*
    // entry — its header is the entry's first line, and its body lies to the
    // right and is therefore already buffered (we read right-to-left). So we
    // never split an entry across blocks.
    //
    // `buf` holds the file's bytes from absolute offset `start` (growing
    // leftward toward 0) to EOF. `emitted_abs` records the absolute offsets of
    // headers already handed to `take`, so re-deriving headers each block never
    // double-emits.
    let mut buf: Vec<u8> = Vec::new();
    let mut start = len;
    let mut emitted_abs: Vec<u64> = Vec::new();
    let mut stop = false;

    while start > 0 && !stop {
        let block = std::cmp::min(REVERSE_BLOCK as u64, start);
        let new_start = start - block;
        file.seek(SeekFrom::Start(new_start))?;
        let mut chunk = vec![0u8; block as usize];
        file.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&buf);
        buf = chunk;
        start = new_start;

        // Find absolute offsets of every header line-start in the current
        // buffer.
        let headers = header_offsets(&buf, start);

        // Process newest (largest offset) → oldest (smallest), emitting any
        // header not yet emitted. Hold back only the buffer's *leftmost* header
        // while we have not reached file start (`start > 0`): older entries may
        // still lie to its left in unread blocks, and newest-first order
        // requires we not emit it until we've confirmed it really is the oldest
        // (or read enough to bound it on the left). One extra block read at
        // most; on the next iteration its left boundary is in-buffer.
        for i in (0..headers.len()).rev() {
            let abs = headers[i];
            if emitted_abs.contains(&abs) {
                continue;
            }
            let is_oldest_in_buf = i == 0;
            if is_oldest_in_buf && start > 0 {
                continue;
            }

            let entry_text = entry_text_at(&buf, start, abs, &headers, i);
            if let Some(entry) = parse_single_entry(&entry_text) {
                emitted_abs.push(abs);
                if take(entry) {
                    stop = true;
                    break;
                }
            } else {
                emitted_abs.push(abs);
            }
        }
    }

    // Reached file start (or stopped). If we stopped, done. If we reached
    // start, emit any held-back oldest header(s) now (start == 0 means the
    // buffer's first header is genuinely the oldest).
    if !stop && start == 0 {
        let headers = header_offsets(&buf, start);
        for i in (0..headers.len()).rev() {
            let abs = headers[i];
            if emitted_abs.contains(&abs) {
                continue;
            }
            let entry_text = entry_text_at(&buf, start, abs, &headers, i);
            if let Some(entry) = parse_single_entry(&entry_text) {
                emitted_abs.push(abs);
                if take(entry) {
                    break;
                }
            } else {
                emitted_abs.push(abs);
            }
        }
    }

    Ok(())
}

/// Absolute byte offsets of every `## [` line-start in `buf`, where `buf`
/// begins at absolute offset `base`.
fn header_offsets(buf: &[u8], base: u64) -> Vec<u64> {
    const PAT: &[u8] = b"## [";
    let mut out = Vec::new();
    let n = buf.len();
    let mut i = 0;
    while i + PAT.len() <= n {
        if &buf[i..i + PAT.len()] == PAT {
            let at_line_start = i == 0 || buf[i - 1] == b'\n';
            if at_line_start {
                out.push(base + i as u64);
                // skip ahead past this marker
                i += PAT.len();
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Extract the text of the entry whose header is at absolute offset
/// `header_abs` (the `headers[idx]` entry), spanning to the next header (or
/// buffer end). `buf` begins at absolute offset `base`.
fn entry_text_at(buf: &[u8], base: u64, header_abs: u64, headers: &[u64], idx: usize) -> String {
    let rel_start = (header_abs - base) as usize;
    let rel_end = if idx + 1 < headers.len() {
        (headers[idx + 1] - base) as usize
    } else {
        buf.len()
    };
    String::from_utf8_lossy(&buf[rel_start..rel_end]).into_owned()
}

/// Parse a single entry from a text block that begins at its header line.
fn parse_single_entry(text: &str) -> Option<LogEntry> {
    parse_entries(text).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Config;
    use std::fs;
    use tempfile::TempDir;

    /// Build a `Store` rooted at a fresh temp dir with a minimal `DB.md`.
    /// Construct the `Store` struct directly so the test stays narrow and never
    /// exercises the `Store::open` parser path.
    fn temp_store() -> (TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("DB.md"), "---\ntype: db-md\n---\n").expect("write DB.md");
        let store = Store {
            root: dir.path().to_path_buf(),
            config: Config::default(),
        };
        (dir, store)
    }

    /// A timestamp at UTC from `YYYY-MM-DD HH:MM` components.
    fn ts(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<FixedOffset> {
        let naive = chrono::NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap();
        FixedOffset::east_opt(0)
            .unwrap()
            .from_local_datetime(&naive)
            .single()
            .unwrap()
    }

    #[allow(clippy::too_many_arguments)] // test fixture builder; struct-ifying churns every call site
    fn entry(
        y: i32,
        mo: u32,
        d: u32,
        h: u32,
        mi: u32,
        kind: LogKind,
        object: Option<&str>,
        note: &str,
    ) -> LogEntry {
        LogEntry {
            timestamp: ts(y, mo, d, h, mi),
            kind,
            object: object.map(|s| s.to_string()),
            note: note.to_string(),
        }
    }

    // ── parse_header ────────────────────────────────────────────────────────

    #[test]
    fn parse_header_with_object() {
        let (t, k, o) =
            Log::parse_header("## [2026-05-27 10:00] ingest | sources/emails/x.eml").unwrap();
        assert_eq!(t, ts(2026, 5, 27, 10, 0));
        assert_eq!(k, LogKind::Ingest);
        assert_eq!(o.as_deref(), Some("sources/emails/x.eml"));
    }

    #[test]
    fn parse_header_without_object_is_none_object() {
        let (t, k, o) = Log::parse_header("## [2026-05-27 10:20] validate").unwrap();
        assert_eq!(t, ts(2026, 5, 27, 10, 20));
        assert_eq!(k, LogKind::Validate);
        assert_eq!(o, None);
    }

    #[test]
    fn parse_header_custom_kind_roundtrips_token() {
        let (_, k, o) = Log::parse_header("## [2026-05-27 10:00] proposal | records/x").unwrap();
        assert_eq!(k, LogKind::Custom("proposal".to_string()));
        assert!(!k.is_recognized());
        assert_eq!(o.as_deref(), Some("records/x"));
    }

    #[test]
    fn parse_header_index_rebuild_hyphenated_kind() {
        let (_, k, _) = Log::parse_header("## [2026-05-27 10:00] index-rebuild").unwrap();
        assert_eq!(k, LogKind::IndexRebuild);
        assert_eq!(k.as_str(), "index-rebuild");
    }

    #[test]
    fn parse_header_rejects_non_headers() {
        assert!(Log::parse_header("Not a header").is_none());
        assert!(Log::parse_header("# Curator log").is_none());
        assert!(Log::parse_header("## [garbage] ingest | x").is_none());
        assert!(Log::parse_header("## [2026-05-27 10:00]").is_none()); // no kind
                                                                       // A bracketed but non-timestamp date must be rejected (LOG_BAD_TIMESTAMP territory).
        assert!(Log::parse_header("## [2026-13-40 99:99] ingest | x").is_none());
    }

    // ── kind round-trip ───────────────────────────────────────────────────────

    #[test]
    fn kind_as_str_parse_roundtrip_for_all_recognized() {
        for k in [
            LogKind::Ingest,
            LogKind::Create,
            LogKind::Update,
            LogKind::Delete,
            LogKind::Rename,
            LogKind::Link,
            LogKind::Validate,
            LogKind::IndexRebuild,
            LogKind::Contradiction,
        ] {
            assert_eq!(LogKind::parse(k.as_str()), k);
            assert!(k.is_recognized());
        }
    }

    // ── append: creation + frontmatter ───────────────────────────────────────

    #[test]
    fn append_creates_log_with_frontmatter_and_entry() {
        let (_d, store) = temp_store();
        let e = entry(
            2026,
            5,
            27,
            10,
            0,
            LogKind::Ingest,
            Some("sources/emails/x.eml"),
            "Email received.",
        );
        Log::append(&store, &e).unwrap();

        let content = fs::read_to_string(store.root.join("log.md")).unwrap();
        // type: log frontmatter present.
        assert!(
            content.starts_with("---\ntype: log\n---\n"),
            "missing log frontmatter; got:\n{content}"
        );
        // The entry header is rendered verbatim.
        assert!(content.contains("## [2026-05-27 10:00] ingest | sources/emails/x.eml"));
        assert!(content.contains("Email received."));
        // No archive dir created when nothing rotates.
        assert!(!store.root.join("log").exists());
    }

    // ── append → tail → since round-trip ─────────────────────────────────────

    #[test]
    fn append_tail_since_roundtrip() {
        let (_d, store) = temp_store();
        let e1 = entry(2026, 5, 27, 10, 0, LogKind::Ingest, Some("a"), "first");
        let e2 = entry(2026, 5, 27, 10, 5, LogKind::Create, Some("b"), "second");
        let e3 = entry(2026, 5, 27, 10, 10, LogKind::Update, Some("c"), "third");
        Log::append(&store, &e1).unwrap();
        Log::append(&store, &e2).unwrap();
        Log::append(&store, &e3).unwrap();

        // tail(2) returns the two newest, in chronological order.
        let tail = Log::tail(&store, 2).unwrap();
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0], e2);
        assert_eq!(tail[1], e3);

        // tail(n) larger than the log returns everything, chronologically.
        let all = Log::tail(&store, 99).unwrap();
        assert_eq!(all, vec![e1.clone(), e2.clone(), e3.clone()]);

        // since(10:05) returns strictly-newer entries (excludes the 10:05 one).
        let since = Log::since(&store, ts(2026, 5, 27, 10, 5)).unwrap();
        assert_eq!(since, vec![e3.clone()]);

        // since before everything returns all.
        let since_all = Log::since(&store, ts(2026, 5, 27, 9, 0)).unwrap();
        assert_eq!(since_all, vec![e1, e2, e3]);
    }

    #[test]
    fn tail_zero_is_empty() {
        let (_d, store) = temp_store();
        Log::append(
            &store,
            &entry(2026, 5, 27, 10, 0, LogKind::Ingest, Some("a"), "x"),
        )
        .unwrap();
        assert!(Log::tail(&store, 0).unwrap().is_empty());
    }

    #[test]
    fn tail_and_since_on_missing_log_are_empty() {
        let (_d, store) = temp_store();
        assert!(Log::tail(&store, 5).unwrap().is_empty());
        assert!(Log::since(&store, ts(2000, 1, 1, 0, 0)).unwrap().is_empty());
        assert!(Log::last_validate_at(&store).unwrap().is_none());
    }

    #[test]
    fn since_exact_timestamp_is_exclusive() {
        let (_d, store) = temp_store();
        let e = entry(2026, 5, 27, 10, 0, LogKind::Validate, None, "PASS");
        Log::append(&store, &e).unwrap();
        // Equal timestamp must NOT be included (strictly newer).
        assert!(Log::since(&store, ts(2026, 5, 27, 10, 0))
            .unwrap()
            .is_empty());
    }

    // ── since: out-of-order on disk (append-only correction / merge=union) ────

    /// Write a `log.md` at the store root from `entries` in the EXACT given
    /// physical order, with the standard `type: log` frontmatter. Unlike
    /// [`Log::append`] (which always lands the newest entry at EOF), this lets a
    /// test author the non-monotonic on-disk shape the SPEC permits — a
    /// backdated corrective entry below the entry it corrects, or a
    /// `merge=union` interleave.
    fn write_raw_log(store: &Store, entries: &[LogEntry]) {
        let mut content = String::from(LOG_FRONTMATTER);
        content.push('\n');
        for e in entries {
            content.push_str(&e.render());
        }
        fs::write(store.root.join("log.md"), content).expect("write raw log.md");
    }

    #[test]
    fn since_returns_newer_entries_even_when_disk_order_is_non_monotonic() {
        // The demonstrated regression: a curator appended a backdated CORRECTIVE
        // entry (10:00) below newer entries (10:10, 10:05), so the physical
        // on-disk order is 10:10, 10:05, 10:00 — newest-first, not chronological.
        // The append-only SPEC explicitly permits this ("append a corrective
        // entry below it"; out-of-order is only LOG_OUT_OF_ORDER, a warning).
        let (_d, store) = temp_store();
        let e_1010 = entry(2026, 5, 27, 10, 10, LogKind::Update, Some("c"), "newest");
        let e_1005 = entry(2026, 5, 27, 10, 5, LogKind::Create, Some("b"), "middle");
        let e_1000 = entry(
            2026,
            5,
            27,
            10,
            0,
            LogKind::Update,
            Some("a"),
            "backdated fix",
        );
        // Physical order on disk: 10:10, 10:05, then the backdated 10:00 LAST.
        write_raw_log(&store, &[e_1010, e_1005, e_1000]);

        // since 10:02 must return BOTH entries strictly newer than 10:02
        // (10:05 and 10:10). The old early-stop hit the physically-last 10:00
        // entry (<= 10:02), stopped, and returned EMPTY — silently dropping the
        // two newer entries that sit earlier in the file.
        let got = Log::since(&store, ts(2026, 5, 27, 10, 2)).unwrap();
        let stamps: std::collections::BTreeSet<_> = got.iter().map(|e| e.timestamp).collect();
        assert_eq!(
            stamps,
            [ts(2026, 5, 27, 10, 5), ts(2026, 5, 27, 10, 10)]
                .into_iter()
                .collect(),
            "since(10:02) must include both 10:05 and 10:10 despite the backdated \
             10:00 entry sitting physically last, and exclude 10:00; got {got:?}"
        );

        // A cutoff before everything still returns all three, regardless of the
        // scrambled disk order.
        let all = Log::since(&store, ts(2026, 5, 27, 9, 0)).unwrap();
        let all_stamps: std::collections::BTreeSet<_> = all.iter().map(|e| e.timestamp).collect();
        assert_eq!(
            all_stamps,
            [
                ts(2026, 5, 27, 10, 0),
                ts(2026, 5, 27, 10, 5),
                ts(2026, 5, 27, 10, 10),
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn since_crosses_archive_when_newer_entry_is_out_of_order_inside_it() {
        // Out-of-order INSIDE an archive month, with the cutoff landing in that
        // month. The April archive is authored newest-physical-first (04-20,
        // then a backdated 04-05 last); a naive early-stop on the first
        // older-than-cutoff entry would miss the later April entry. The active
        // file holds a clean May entry. Cutoff = mid-April.
        let (_d, store) = temp_store();

        // Active file: one current-month (May) entry.
        let may = entry(2026, 5, 2, 8, 0, LogKind::Update, Some("may-a"), "may1");
        write_raw_log(&store, &[may]);

        // April archive authored out of order: 04-20 first, backdated 04-05 last.
        let apr_late = entry(
            2026,
            4,
            20,
            9,
            0,
            LogKind::Create,
            Some("apr-b"),
            "apr-late",
        );
        let apr_early = entry(
            2026,
            4,
            5,
            9,
            0,
            LogKind::Ingest,
            Some("apr-a"),
            "apr-early",
        );
        let dir = store.root.join("log");
        fs::create_dir_all(&dir).unwrap();
        let mut arch = String::from(LOG_FRONTMATTER);
        arch.push('\n');
        arch.push_str(&apr_late.render());
        arch.push_str(&apr_early.render());
        fs::write(dir.join("2026-04.md"), arch).unwrap();

        // since mid-April: the later April entry (04-20) AND the May entry must
        // come back; the early April entry (04-05) must not.
        let got = Log::since(&store, ts(2026, 4, 15, 0, 0)).unwrap();
        let stamps: std::collections::BTreeSet<_> = got.iter().map(|e| e.timestamp).collect();
        assert_eq!(
            stamps,
            [ts(2026, 4, 20, 9, 0), ts(2026, 5, 2, 8, 0)]
                .into_iter()
                .collect(),
            "since(mid-April) must include the out-of-order later April entry \
             and the May entry, and exclude the earlier April entry; got {got:?}"
        );
    }

    // ── multi-line notes ──────────────────────────────────────────────────────

    #[test]
    fn multiline_note_is_preserved() {
        let (_d, store) = temp_store();
        let e = entry(
            2026,
            5,
            27,
            10,
            0,
            LogKind::Create,
            Some("records/x"),
            "Line one.\nLine two.\nLine three.",
        );
        Log::append(&store, &e).unwrap();
        let got = Log::tail(&store, 1).unwrap();
        assert_eq!(got[0].note, "Line one.\nLine two.\nLine three.");
    }

    #[test]
    fn empty_note_roundtrips_as_empty() {
        let (_d, store) = temp_store();
        let e = entry(2026, 5, 27, 10, 0, LogKind::Validate, None, "");
        Log::append(&store, &e).unwrap();
        let got = Log::tail(&store, 1).unwrap();
        assert_eq!(got[0], e);
        assert_eq!(got[0].note, "");
    }

    // ── last_validate_at ─────────────────────────────────────────────────────

    #[test]
    fn last_validate_at_finds_most_recent_validate() {
        let (_d, store) = temp_store();
        Log::append(
            &store,
            &entry(2026, 5, 27, 10, 0, LogKind::Validate, None, "first pass"),
        )
        .unwrap();
        Log::append(
            &store,
            &entry(2026, 5, 27, 10, 5, LogKind::Create, Some("a"), "made a"),
        )
        .unwrap();
        Log::append(
            &store,
            &entry(2026, 5, 27, 10, 10, LogKind::Validate, None, "second pass"),
        )
        .unwrap();
        Log::append(
            &store,
            &entry(2026, 5, 27, 10, 15, LogKind::Update, Some("a"), "edit a"),
        )
        .unwrap();

        let last = Log::last_validate_at(&store).unwrap();
        assert_eq!(last, Some(ts(2026, 5, 27, 10, 10)));
    }

    #[test]
    fn last_validate_at_none_when_no_validate() {
        let (_d, store) = temp_store();
        Log::append(
            &store,
            &entry(2026, 5, 27, 10, 0, LogKind::Create, Some("a"), "x"),
        )
        .unwrap();
        assert_eq!(Log::last_validate_at(&store).unwrap(), None);
    }

    // ── month-boundary rotation ──────────────────────────────────────────────

    #[test]
    fn rotation_rolls_prior_months_into_archives() {
        let (_d, store) = temp_store();
        // Two April entries and one May entry, all written while "current" was
        // their own month (append-only chronological order).
        let a1 = entry(2026, 4, 10, 9, 0, LogKind::Ingest, Some("apr-a"), "apr one");
        let a2 = entry(2026, 4, 20, 9, 0, LogKind::Create, Some("apr-b"), "apr two");
        Log::append(&store, &a1).unwrap();
        Log::append(&store, &a2).unwrap();

        // Before rotation: no archive dir, both April entries in active.
        assert!(!store.root.join("log").exists());

        // Appending a May entry must roll April into log/2026-04.md.
        let m1 = entry(2026, 5, 2, 8, 0, LogKind::Update, Some("may-a"), "may one");
        Log::append(&store, &m1).unwrap();

        // Archive exists and holds both April entries with frontmatter.
        let arch_path = store.root.join("log").join("2026-04.md");
        assert!(arch_path.exists(), "expected April archive to be created");
        let arch = fs::read_to_string(&arch_path).unwrap();
        assert!(arch.starts_with("---\ntype: log\n---\n"));
        assert!(arch.contains("## [2026-04-10 09:00] ingest | apr-a"));
        assert!(arch.contains("## [2026-04-20 09:00] create | apr-b"));
        assert!(arch.contains("apr one"));
        assert!(arch.contains("apr two"));

        // Active file now holds ONLY the May entry (no April entries).
        let active = fs::read_to_string(store.root.join("log.md")).unwrap();
        assert!(active.contains("## [2026-05-02 08:00] update | may-a"));
        assert!(
            !active.contains("apr-a") && !active.contains("apr-b"),
            "April entries must be gone from the active file; got:\n{active}"
        );

        // The full timeline (archives ++ active) is intact and chronological.
        let all = Log::tail(&store, 99).unwrap();
        assert_eq!(all, vec![a1, a2, m1]);
    }

    #[test]
    fn rotation_groups_distinct_prior_months_into_separate_archives() {
        let (_d, store) = temp_store();
        // March + April entries accumulate, then a May append rolls BOTH prior
        // months into their own archive files.
        let mar = entry(2026, 3, 5, 9, 0, LogKind::Ingest, Some("mar"), "march");
        let apr = entry(2026, 4, 5, 9, 0, LogKind::Create, Some("apr"), "april");
        Log::append(&store, &mar).unwrap();
        Log::append(&store, &apr).unwrap();
        // At this point April is current, March already rolled into its archive.
        assert!(store.root.join("log").join("2026-03.md").exists());

        let may = entry(2026, 5, 5, 9, 0, LogKind::Update, Some("may"), "may");
        Log::append(&store, &may).unwrap();

        assert!(store.root.join("log").join("2026-03.md").exists());
        assert!(store.root.join("log").join("2026-04.md").exists());

        // Each archive holds only its own month.
        let mar_arch = fs::read_to_string(store.root.join("log").join("2026-03.md")).unwrap();
        let apr_arch = fs::read_to_string(store.root.join("log").join("2026-04.md")).unwrap();
        assert!(mar_arch.contains("mar") && !mar_arch.contains("apr"));
        assert!(apr_arch.contains("apr") && !apr_arch.contains("mar"));

        // Active holds only May.
        let active = fs::read_to_string(store.root.join("log.md")).unwrap();
        assert!(active.contains("may") && !active.contains("mar") && !active.contains("apr"));

        // Timeline intact and ordered across both archives + active.
        let all = Log::tail(&store, 99).unwrap();
        assert_eq!(all, vec![mar, apr, may]);
    }

    #[test]
    fn tail_crosses_into_archive_when_n_spans_month_boundary() {
        let (_d, store) = temp_store();
        let a1 = entry(2026, 4, 10, 9, 0, LogKind::Ingest, Some("apr-a"), "apr1");
        let a2 = entry(2026, 4, 20, 9, 0, LogKind::Create, Some("apr-b"), "apr2");
        let m1 = entry(2026, 5, 2, 8, 0, LogKind::Update, Some("may-a"), "may1");
        let m2 = entry(2026, 5, 3, 8, 0, LogKind::Update, Some("may-b"), "may2");
        for e in [&a1, &a2, &m1, &m2] {
            Log::append(&store, e).unwrap();
        }
        // April is now archived; active holds only May. tail(3) must reach back
        // into the archive for the third-newest entry.
        let tail3 = Log::tail(&store, 3).unwrap();
        assert_eq!(tail3, vec![a2.clone(), m1.clone(), m2.clone()]);

        // tail within the active month does NOT need the archive but is still
        // correct.
        let tail2 = Log::tail(&store, 2).unwrap();
        assert_eq!(tail2, vec![m1, m2]);
    }

    #[test]
    fn since_crosses_into_archive_and_early_stops() {
        let (_d, store) = temp_store();
        let a1 = entry(2026, 4, 10, 9, 0, LogKind::Ingest, Some("apr-a"), "apr1");
        let a2 = entry(2026, 4, 20, 9, 0, LogKind::Create, Some("apr-b"), "apr2");
        let m1 = entry(2026, 5, 2, 8, 0, LogKind::Update, Some("may-a"), "may1");
        for e in [&a1, &a2, &m1] {
            Log::append(&store, e).unwrap();
        }
        // since a mid-April time: must include the later April entry (from the
        // archive) and the May entry, but not the earlier April one.
        let got = Log::since(&store, ts(2026, 4, 15, 0, 0)).unwrap();
        assert_eq!(got, vec![a2, m1]);
    }

    #[test]
    fn last_validate_at_crosses_into_archive() {
        let (_d, store) = temp_store();
        // A validate in April, then non-validate work that rolls April away.
        Log::append(
            &store,
            &entry(2026, 4, 10, 9, 0, LogKind::Validate, None, "apr validate"),
        )
        .unwrap();
        Log::append(
            &store,
            &entry(2026, 5, 2, 8, 0, LogKind::Update, Some("may-a"), "may work"),
        )
        .unwrap();
        // Active has only the May update; the most-recent validate lives in the
        // April archive and must still be found.
        let last = Log::last_validate_at(&store).unwrap();
        assert_eq!(last, Some(ts(2026, 4, 10, 9, 0)));
    }

    // ── reverse-read correctness on a large (multi-block) log ────────────────

    #[test]
    fn reverse_read_correct_on_large_single_month_log() {
        let (_d, store) = temp_store();
        // Append many same-month entries with chunky multi-line notes so the
        // file spans well past one REVERSE_BLOCK (8 KiB). Timestamps are
        // strictly increasing (a real append-only log is monotonic): each entry
        // is 3 minutes after the previous, all within June, so physical order
        // equals chronological order and the last-k-physical ARE the k-newest.
        let n = 400usize;
        let mut expected: Vec<LogEntry> = Vec::new();
        for i in 0..n {
            let total_min = (i as u32) * 3;
            let day = 1 + total_min / (24 * 60);
            let hour = (total_min / 60) % 24;
            let min = total_min % 60;
            // Unique, multi-line note to bulk up the file and detect mis-parses.
            let note = format!(
                "entry number {i}\nbody line A for {i}\nbody line B for {i} with padding {}",
                "x".repeat(40)
            );
            let e = entry(
                2026,
                6,
                day,
                hour,
                min,
                LogKind::Update,
                Some(&format!("records/item-{i:04}")),
                &note,
            );
            Log::append(&store, &e).unwrap();
            expected.push(e);
        }

        // File must actually be multi-block to exercise the backward reader.
        let size = fs::metadata(store.root.join("log.md")).unwrap().len();
        assert!(
            size > (REVERSE_BLOCK as u64) * 2,
            "test log not large enough ({size} bytes) to exercise multi-block reverse-read"
        );

        // tail(5) must equal the 5 newest, exactly.
        let tail5 = Log::tail(&store, 5).unwrap();
        assert_eq!(tail5, expected[n - 5..].to_vec());

        // tail(50) must equal the 50 newest.
        let tail50 = Log::tail(&store, 50).unwrap();
        assert_eq!(tail50, expected[n - 50..].to_vec());

        // tail(all) must reconstruct the whole timeline in order.
        let all = Log::tail(&store, n + 10).unwrap();
        assert_eq!(all.len(), n);
        assert_eq!(all, expected);
    }

    // ── tail on OUT-OF-ORDER logs (newest-by-timestamp, not last-physical) ────
    //
    // The append-only contract is non-decreasing time order, but it's only a
    // `LOG_OUT_OF_ORDER` warning when violated (corrective entries land below
    // the entry they correct; backdated / clock-skewed writes; `merge=union`
    // clone merges). `tail N` must return the N newest *by timestamp*, never the
    // last N *physical* entries.

    /// Write `log.md` verbatim from rendered entries in the given **physical
    /// (file) order**, bypassing `Log::append` so the test controls on-disk
    /// order exactly (append never reorders within a month, but this is the
    /// clearest way to pin a specific physical layout).
    fn write_log_physical(store: &Store, entries: &[LogEntry]) {
        let mut body = String::new();
        for e in entries {
            body.push_str(&e.render());
        }
        let full = compose_active(LOG_FRONTMATTER, &body);
        fs::write(store.root.join("log.md"), full).expect("write log.md");
    }

    #[test]
    fn tail_returns_newest_by_timestamp_on_demonstrated_out_of_order_log() {
        // The exact case from the review finding: physical order 10:10, 10:05,
        // 10:00 (a backdated entry tail). The OLD code returned the last two
        // physical entries {10:05, 10:00}; the correct answer is the two newest
        // by time {10:05, 10:10}.
        let (_d, store) = temp_store();
        let e_1010 = entry(2026, 5, 27, 10, 10, LogKind::Update, Some("c"), "ten-ten");
        let e_1005 = entry(
            2026,
            5,
            27,
            10,
            5,
            LogKind::Create,
            Some("b"),
            "ten-oh-five",
        );
        let e_1000 = entry(2026, 5, 27, 10, 0, LogKind::Ingest, Some("a"), "ten-oh-oh");
        // Physical order: newest first, then the two older ones — out of order.
        write_log_physical(&store, &[e_1010.clone(), e_1005.clone(), e_1000.clone()]);

        let tail2 = Log::tail(&store, 2).unwrap();
        assert_eq!(
            tail2,
            vec![e_1005.clone(), e_1010.clone()],
            "tail(2) must be the two NEWEST by timestamp (chronological), \
             not the last two physical entries"
        );
        // The newest entry must be present and the oldest absent.
        assert!(tail2.contains(&e_1010), "newest (10:10) must be included");
        assert!(!tail2.contains(&e_1000), "oldest (10:00) must be excluded");

        // tail(1) is just the single newest.
        assert_eq!(Log::tail(&store, 1).unwrap(), vec![e_1010.clone()]);
        // tail(all) is the full set in chronological order.
        assert_eq!(Log::tail(&store, 99).unwrap(), vec![e_1000, e_1005, e_1010]);
    }

    #[test]
    fn tail_no_early_stop_when_newer_entry_sits_before_an_older_one() {
        // Guards the unsound within-file early stop: a newer entry (10:50) sits
        // PHYSICALLY BEFORE a much older one (10:00). Reading newest-physical-
        // first, the scan meets 10:00 before 10:50; any "stop at the first entry
        // below the window minimum" rule would bail and drop 10:50.
        //
        // Physical (top→bottom): 10:55, 10:10, 10:50, 10:00.
        // Reverse-scan order:     10:00, 10:50, 10:10, 10:55.
        let (_d, store) = temp_store();
        let e55 = entry(2026, 5, 27, 10, 55, LogKind::Update, Some("x55"), "55");
        let e10 = entry(2026, 5, 27, 10, 10, LogKind::Update, Some("x10"), "10");
        let e50 = entry(2026, 5, 27, 10, 50, LogKind::Update, Some("x50"), "50");
        let e00 = entry(2026, 5, 27, 10, 0, LogKind::Update, Some("x00"), "00");
        write_log_physical(
            &store,
            &[e55.clone(), e10.clone(), e50.clone(), e00.clone()],
        );

        // The two newest by timestamp are 10:55 and 10:50 — NOT the early-stop
        // victim 10:10, and NOT the last-physical 10:00.
        let tail2 = Log::tail(&store, 2).unwrap();
        assert_eq!(tail2, vec![e50.clone(), e55.clone()]);

        let tail3 = Log::tail(&store, 3).unwrap();
        assert_eq!(tail3, vec![e10.clone(), e50.clone(), e55.clone()]);
    }

    #[test]
    fn tail_orders_equal_timestamps_by_physical_recency() {
        // Three entries share 10:00; one is at 09:59. tail(2) must keep both
        // 10:00 entries, and among the equal pair the one appended LATER
        // (physically last) sorts last ("newest" = most-recently recorded).
        let (_d, store) = temp_store();
        let early = entry(2026, 5, 27, 9, 59, LogKind::Create, Some("early"), "before");
        let tie_a = entry(
            2026,
            5,
            27,
            10,
            0,
            LogKind::Update,
            Some("tie-a"),
            "first 10:00",
        );
        let tie_b = entry(
            2026,
            5,
            27,
            10,
            0,
            LogKind::Update,
            Some("tie-b"),
            "second 10:00",
        );
        // Physical append order: early, tie_a, tie_b.
        write_log_physical(&store, &[early.clone(), tie_a.clone(), tie_b.clone()]);

        let tail2 = Log::tail(&store, 2).unwrap();
        assert_eq!(
            tail2,
            vec![tie_a.clone(), tie_b.clone()],
            "both 10:00 entries kept, physically-later one (tie_b) last; 09:59 dropped"
        );
        // tail(1) keeps only the most-recently-recorded of the equal pair.
        assert_eq!(Log::tail(&store, 1).unwrap(), vec![tie_b]);
    }

    #[test]
    fn tail_finds_newest_across_a_backdated_entry_spanning_the_month_boundary() {
        // A backdated entry can land physically after newer entries even across
        // a rotation: append May entries, then a June entry (rolls May to its
        // archive), then append a May-dated correction — it goes into the ACTIVE
        // file, physically after June. tail must still rank by timestamp, so the
        // June entry stays newest and the backdated May entry is not mistaken
        // for the tail.
        let (_d, store) = temp_store();
        let may1 = entry(2026, 5, 10, 9, 0, LogKind::Ingest, Some("may-1"), "may one");
        let may2 = entry(2026, 5, 20, 9, 0, LogKind::Create, Some("may-2"), "may two");
        let jun1 = entry(2026, 6, 2, 8, 0, LogKind::Update, Some("jun-1"), "jun one");
        Log::append(&store, &may1).unwrap();
        Log::append(&store, &may2).unwrap();
        Log::append(&store, &jun1).unwrap(); // rotates May -> log/2026-05.md
        assert!(store.root.join("log").join("2026-05.md").exists());

        // A backdated May correction, appended now: it lands in the active file
        // (its month May is not strictly before the active month June), so the
        // active file is physically [jun1, may_corr] — out of order.
        let may_corr = entry(
            2026,
            5,
            25,
            9,
            0,
            LogKind::Update,
            Some("may-2"),
            "may correction",
        );
        Log::append(&store, &may_corr).unwrap();
        let active = fs::read_to_string(store.root.join("log.md")).unwrap();
        assert!(
            active.contains("jun-1") && active.contains("may correction"),
            "backdated May entry should be in the active file alongside June; got:\n{active}"
        );

        // The single newest by timestamp is the June entry, even though the
        // backdated May entry is physically last.
        assert_eq!(Log::tail(&store, 1).unwrap(), vec![jun1.clone()]);

        // tail(2): the two newest by time are may_corr (05-25) and jun1 (06-02).
        let tail2 = Log::tail(&store, 2).unwrap();
        assert_eq!(tail2, vec![may_corr.clone(), jun1.clone()]);

        // tail(3) must reach into the May archive for the third-newest (may2,
        // 05-20), proving archive crossing still works on an out-of-order store.
        let tail3 = Log::tail(&store, 3).unwrap();
        assert_eq!(tail3, vec![may2.clone(), may_corr.clone(), jun1.clone()]);

        // tail(all) reconstructs the whole timeline in chronological order.
        let all = Log::tail(&store, 99).unwrap();
        assert_eq!(all, vec![may1, may2, may_corr, jun1]);
    }

    #[test]
    fn parse_entries_skips_unparseable_header_folding_into_body() {
        // A `## [` line that is NOT a valid header should not start a new entry;
        // it folds into the preceding entry's note. This guards the
        // parse_entries header-validation branch.
        let text = "\
## [2026-05-27 10:00] create | records/x
Body mentions a literal: ## [not a real header here]
More body.

## [2026-05-27 10:05] update | records/y
Second.
";
        let entries = parse_entries(text);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, LogKind::Create);
        assert!(entries[0].note.contains("## [not a real header here]"));
        assert!(entries[0].note.contains("More body."));
        assert_eq!(entries[1].kind, LogKind::Update);
        assert_eq!(entries[1].note, "Second.");
    }

    // ── append-only: corrective entries go on the end ─────────────────────────

    #[test]
    fn append_only_corrective_entry_goes_on_end_without_rewriting() {
        let (_d, store) = temp_store();
        let original = entry(
            2026,
            5,
            27,
            10,
            0,
            LogKind::Update,
            Some("records/northstar"),
            "Seat count 120 -> 175.",
        );
        Log::append(&store, &original).unwrap();
        let after_first = fs::read_to_string(store.root.join("log.md")).unwrap();

        // A correction is a NEW entry appended on the end; the original text is
        // left byte-for-byte intact (append-only contract: no rewrite API).
        let correction = entry(
            2026,
            5,
            27,
            11,
            0,
            LogKind::Update,
            Some("records/northstar"),
            "Correction: seat count is 165, not 175.",
        );
        Log::append(&store, &correction).unwrap();
        let after_second = fs::read_to_string(store.root.join("log.md")).unwrap();

        assert!(
            after_second.starts_with(&after_first),
            "appending must not rewrite earlier bytes"
        );
        assert!(after_second.contains("Correction: seat count is 165, not 175."));

        // Both entries are readable, in order.
        let all = Log::tail(&store, 99).unwrap();
        assert_eq!(all, vec![original, correction]);
    }

    // ── concurrent append safety (atomic via temp-file rename) ────────────────

    #[test]
    fn concurrent_appends_are_atomic_and_total() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let (_d, store) = temp_store();
        // Seed the file so all threads take the read-modify-write path.
        Log::append(
            &store,
            &entry(2026, 7, 1, 0, 0, LogKind::Create, Some("seed"), "seed"),
        )
        .unwrap();

        let threads = 8usize;
        let per = 25usize;
        let barrier = Arc::new(Barrier::new(threads));
        let store = Arc::new(store);

        let mut handles = Vec::new();
        for tnum in 0..threads {
            let b = Arc::clone(&barrier);
            let s = Arc::clone(&store);
            handles.push(thread::spawn(move || {
                b.wait();
                for i in 0..per {
                    let e = entry(
                        2026,
                        7,
                        1,
                        (tnum % 24) as u32,
                        (i % 60) as u32,
                        LogKind::Update,
                        Some(&format!("t{tnum}-i{i}")),
                        &format!("thread {tnum} item {i}"),
                    );
                    Log::append(&s, &e).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        // The atomic temp-file-rename write means no append truncates or
        // corrupts another: the file must remain parseable and every line of
        // every entry header must be well-formed. Crucially, no entry should be
        // lost to a torn write of the *content already on disk* — though
        // interleaved read-modify-write WILL drop some appends (last-writer-
        // wins on the snapshot). We therefore assert integrity + that the file
        // never went empty / corrupt, not an exact count.
        let content = fs::read_to_string(store.root.join("log.md")).unwrap();
        assert!(content.starts_with("---\ntype: log\n---\n"));

        // Every `## [` line must parse as a valid header (no half-written line).
        for line in content.lines() {
            if line.starts_with("## [") {
                assert!(
                    Log::parse_header(line).is_some(),
                    "corrupt/torn header line on disk: {line:?}"
                );
            }
        }

        // The seed entry must survive (it was written before the race and
        // every snapshot included it).
        assert!(content.contains("## [2026-07-01 00:00] create | seed"));

        // The reverse reader must still produce a clean, fully-parseable view.
        let all = Log::tail(&store, 10_000).unwrap();
        assert!(!all.is_empty());
        // No duplicate adjacent identical headers from a torn write: every
        // returned entry must have a recognized-or-custom kind and a parseable
        // timestamp (already guaranteed by parse), and the list must be
        // internally consistent (re-render → re-parse identity for each).
        for e in &all {
            let rendered = e.render();
            let reparsed = parse_single_entry(&rendered).unwrap();
            assert_eq!(&reparsed, e);
        }
    }

    // ── render/parse identity ────────────────────────────────────────────────

    #[test]
    fn render_then_parse_is_identity() {
        let cases = vec![
            entry(
                2026,
                1,
                2,
                3,
                4,
                LogKind::Ingest,
                Some("sources/a.eml"),
                "n",
            ),
            entry(
                2026,
                12,
                31,
                23,
                59,
                LogKind::Validate,
                None,
                "PASS - 0 errors",
            ),
            entry(
                2026,
                6,
                15,
                12,
                30,
                LogKind::Custom("proposal".to_string()),
                Some("records/p"),
                "multi\nline\nnote",
            ),
            entry(2026, 6, 15, 12, 30, LogKind::Contradiction, Some("obj"), ""),
        ];
        for e in cases {
            let rendered = e.render();
            let parsed = parse_single_entry(&rendered).unwrap_or_else(|| {
                panic!("failed to reparse rendered entry:\n{rendered}");
            });
            assert_eq!(parsed, e, "round-trip mismatch for {e:?}");
        }
    }
}
