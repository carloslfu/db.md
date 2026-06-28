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
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, FixedOffset, NaiveDateTime, TimeZone, Utc};

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
    /// separated. The note is emitted with header-shaped continuation lines
    /// **escaped** (see [`escape_note_line`]) so a note line that happens to
    /// match the entry-header shape (`## [YYYY-MM-DD HH:MM] <kind> | <obj>`) can
    /// never be mistaken for a real entry header on readback or on the next
    /// rotation. The escape round-trips exactly through [`unescape_note_line`].
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
        // Trim only the structural line terminators (`\n`/`\r`) — the trailing
        // blank line separating entries is appended below, so a note's own
        // trailing newlines would otherwise stack up and shift on every
        // re-render. Spaces and tabs are legitimate note *content* and must be
        // preserved verbatim, so the round-trip is exact: readback
        // (`parse_entries`) trims the same `['\n', '\r']` set and no more, and a
        // note ending in a space (`"note 0 "`) must reconstruct unchanged.
        let note = self.note.trim_end_matches(['\n', '\r']);
        if !note.is_empty() {
            // Escape per line: a note line that parses as an entry header is
            // prefixed so it is no longer at column 0 as `## [` — it stays note
            // body on readback and on rotation, never a fabricated entry.
            for (i, line) in note.split('\n').enumerate() {
                if i > 0 {
                    out.push('\n');
                }
                out.push_str(&escape_note_line(line));
            }
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
    ///
    /// **Concurrency.** `append` is a read-modify-write of the whole active file
    /// (`write_atomic` is atomic at the file level, but the read→render→write
    /// window is not). Two concurrent appenders — the manager and a cron-driven
    /// background system, say — would otherwise both read the same N-entry
    /// snapshot and each write N+1 entries, the second rename clobbering the
    /// first and silently dropping an audit entry. We serialize the whole
    /// read-modify-write under an advisory file lock (`flock`, held for the
    /// duration) so concurrent appends queue instead of racing. The lock is
    /// advisory and process-scoped; it guards the toolkit's own appends, which is
    /// the realistic contention path.
    pub fn append(store: &Store, entry: &LogEntry) -> crate::Result<()> {
        let active = active_log_path(store);

        // Serialize concurrent appends for the whole read-modify-write. Held
        // until `_lock` drops at function exit (covering both the rotation and
        // the plain-append paths). A lock failure is non-fatal: we proceed
        // unlocked rather than refuse to log (best-effort, same posture as the
        // pre-fix behaviour on platforms without advisory locks).
        let _lock = AppendLock::acquire(&active);

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

            // A rotation is two non-atomic durable writes (archive append, then
            // active trim). The marker disambiguates a crash-retry re-roll from a
            // fresh rotation so a genuinely-distinct same-minute entry is never
            // dropped (see `rotation_marker_path`). `recovering` is captured
            // BEFORE we (re)write the marker, so the current attempt's archive
            // append uses the right mode; the marker only changes what a LATER
            // retry sees.
            let marker = rotation_marker_path(store);
            let recovering = marker.exists();

            if !by_month.is_empty() {
                // Roll each prior month into its archive (atomic per-file),
                // appending to any existing archive for that month.
                let dir = archive_dir(store);
                fs::create_dir_all(&dir)?;
                // Mark the rotation in-flight so a crash before the active trim
                // is recoverable as a re-roll (deduped), not re-appended.
                if !recovering {
                    fs::write(&marker, b"")?;
                }
                for ((y, m), month_entries) in &by_month {
                    let path = archive_path(store, *y, *m);
                    append_to_archive(&path, month_entries, recovering)?;
                }

                // Rewrite the active file to the kept (current-month) entries
                // plus the new entry — atomically.
                let mut body = String::new();
                for e in &keep {
                    body.push_str(&e.render());
                }
                body.push_str(&entry.render());
                let full = compose_active(&header, &body);
                crate::fsx::write_atomic(&active, full.as_bytes())?;
                // Rotation committed (active trimmed): clear the in-flight marker.
                let _ = fs::remove_file(&marker);
                return Ok(());
            }

            // No rotation needed. If a stale marker lingers (a crash that trimmed
            // the active file but never deleted the marker), clear it so the next
            // real rotation is treated as fresh, not stuck in recovery mode.
            if recovering {
                let _ = fs::remove_file(&marker);
            }
            // Plain atomic append of the rendered entry.
            let mut full = content;
            if !full.ends_with('\n') {
                full.push('\n');
            }
            full.push_str(&entry.render());
            crate::fsx::write_atomic(&active, full.as_bytes())?;
            Ok(())
        } else {
            // Fresh log: frontmatter + the single entry.
            if let Some(parent) = active.parent() {
                fs::create_dir_all(parent)?;
            }
            let body = entry.render();
            let full = compose_active(LOG_FRONTMATTER, &body);
            crate::fsx::write_atomic(&active, full.as_bytes())?;
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
        // Active↔archive overlap dedup, narrowly scoped (see `since`): an
        // interrupted rotation can leave the SAME entry in both the untrimmed
        // active file and its month archive; without suppression it would occupy
        // two window slots and surface twice. We record every ACTIVE entry's
        // identity and suppress only an ARCHIVE entry that matches one — NEVER an
        // active entry against another active entry, nor an archive entry against
        // another archive entry. A global content key over-reaches: on-disk
        // headers are minute-precision, so two genuinely-distinct same-minute
        // appends share an identity and a global dedup silently dropped the
        // second on read.
        let mut active_seen: std::collections::HashSet<EntryKey> = std::collections::HashSet::new();

        // Active file: scan fully (current-month-bounded by rotation). Record
        // every identity for overlap detection, but consider every entry — a
        // same-minute duplicate WITHIN the active file is two distinct appends.
        let active = active_log_path(store);
        if active.exists() {
            reverse_collect(&active, |e| {
                active_seen.insert(entry_key(&e));
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
                // Suppress only the active↔archive crash-retry overlap; keep
                // every distinct same-minute archive entry (archives are never
                // deduped against each other).
                if !active_seen.contains(&entry_key(&e)) {
                    window.consider(e);
                }
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
        // Active↔archive overlap dedup, narrowly scoped. An interrupted rotation
        // (archive write committed, active rewrite not) leaves the same entries
        // in BOTH the untrimmed active file and the archive; without suppression
        // each comes back twice. We record ACTIVE identities and suppress only an
        // ARCHIVE entry that matches one — never active-vs-active or
        // archive-vs-archive. A global content key would over-reach: on-disk
        // headers are minute-precision, so two genuinely-distinct same-minute
        // appends share an identity, and a global dedup silently under-reported
        // the second.
        let mut active_seen: std::collections::HashSet<EntryKey> = std::collections::HashSet::new();

        // Active file: scan fully, no early stop (out-of-order safe). Collect
        // every in-window entry (a same-minute duplicate within the active file
        // is two distinct appends), recording identities for overlap detection.
        let active = active_log_path(store);
        if active.exists() {
            reverse_collect(&active, |e| {
                if e.timestamp > time {
                    active_seen.insert(entry_key(&e));
                    collected.push(e);
                }
                false
            })?;
        }

        // The cutoff's own (year, month): any archive strictly before it holds
        // only older entries and is skippable. Archive months are bucketed on
        // the UTC calendar (on-disk timestamps are offset-free and re-read as
        // UTC; rotation buckets by the entry's UTC year-month), so the pruning
        // calendar must be UTC too. A non-UTC `since` offset (advertised in the
        // CLI hint, e.g. `…T00:30:00+07:00`) whose local month differs from its
        // UTC month would otherwise prune away an archive holding entries that
        // are strictly newer than `time` — `time.year()/.month()` read the
        // offset-LOCAL calendar, not UTC.
        let cutoff_utc = time.with_timezone(&Utc);
        let cutoff_ym = (cutoff_utc.year(), cutoff_utc.month());

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
                // Suppress only the active↔archive crash-retry overlap; keep
                // every distinct same-minute archive entry.
                if e.timestamp > time && !active_seen.contains(&entry_key(&e)) {
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

/// An advisory, exclusive lock serializing concurrent [`Log::append`] calls.
///
/// Held on a dedicated sibling lock file (`<active>.lock`) rather than on
/// `log.md` itself: `write_atomic` replaces the active file by `rename`, so the
/// active inode changes under us and a lock on its fd would not cover the new
/// file. The lock file is stable, so the lock spans the whole read-modify-write.
///
/// On Unix this is `flock(LOCK_EX)`, released on drop (or implicitly when the
/// process exits / the fd closes, so a crash never strands the lock). The
/// lock file is created if absent and intentionally left on disk between runs
/// (locking it does not depend on its contents). On non-Unix targets the lock
/// is a no-op — db.md's append surface is Unix-targeted, and a missing advisory
/// lock degrades to the pre-fix last-writer-wins, never to incorrectness of a
/// single writer.
struct AppendLock {
    #[cfg(unix)]
    file: Option<File>,
}

impl AppendLock {
    /// Acquire the exclusive append lock for the store whose active log is
    /// `active`. Best-effort: any failure to open or lock the lock file yields
    /// an unlocked guard (we log rather than refuse to log). Blocks until the
    /// lock is granted when another appender holds it.
    fn acquire(active: &Path) -> AppendLock {
        #[cfg(unix)]
        {
            let file = Self::open_and_lock(active);
            AppendLock { file }
        }
        #[cfg(not(unix))]
        {
            let _ = active;
            AppendLock {}
        }
    }

    #[cfg(unix)]
    fn open_and_lock(active: &Path) -> Option<File> {
        use std::os::unix::io::AsRawFd;

        // The lock file lives beside the active log; ensure its parent exists
        // (the fresh-log path may run before `log.md`'s directory is created).
        if let Some(parent) = active.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let lock_path = lock_path_for(active);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .ok()?;

        // Blocking exclusive advisory lock. `flock` is in libc, which every Rust
        // binary links, so the bare `extern "C"` declaration needs no crate dep.
        let rc = unsafe { flock(file.as_raw_fd(), LOCK_EX) };
        if rc != 0 {
            // Could not lock (e.g. a filesystem without flock support): proceed
            // unlocked rather than fail the append.
            return None;
        }
        Some(file)
    }
}

#[cfg(unix)]
impl Drop for AppendLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        if let Some(file) = &self.file {
            // Release explicitly; the fd close on drop would also release it.
            unsafe { flock(file.as_raw_fd(), LOCK_UN) };
        }
    }
}

#[cfg(unix)]
extern "C" {
    fn flock(fd: std::os::raw::c_int, operation: std::os::raw::c_int) -> std::os::raw::c_int;
}

/// `flock` operation: exclusive lock (`LOCK_EX`), blocking.
#[cfg(unix)]
const LOCK_EX: std::os::raw::c_int = 2;
/// `flock` operation: unlock (`LOCK_UN`).
#[cfg(unix)]
const LOCK_UN: std::os::raw::c_int = 8;

/// The advisory-lock sibling path for an active log file (`<name>.lock`).
#[cfg(unix)]
fn lock_path_for(active: &Path) -> PathBuf {
    let mut name = active
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("log.md"));
    name.push(".lock");
    match active.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
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

/// The crash-recovery marker for an in-progress rotation.
///
/// Its **presence** at the start of [`Log::append`] means a prior rotation
/// appended prior-month entries to their archives but may not have trimmed the
/// active file (a crash, or an active-rewrite error, between the two non-atomic
/// durable writes). The retry must then DEDUP the re-rolled entries against the
/// archive so it adds nothing.
///
/// Its **absence** means a fresh rotation: every prior-month entry being rolled
/// is genuinely new to its archive and is appended UNCONDITIONALLY. This is the
/// load-bearing distinction — a content-only dedup cannot tell an idempotent
/// re-roll of one physical entry from a genuinely-distinct same-minute repeat
/// (on-disk headers are minute-precision, so two real appends to the same object
/// in the same minute with the same note render byte-identically). Gating the
/// dedup on "are we recovering a crashed rotation?" lets a backdated duplicate
/// survive while still suppressing a true re-roll.
///
/// Lives in `log/` (toolkit-managed; a dotfile, so never walked, indexed, or
/// validated as content — `list_archives_desc` matches only `YYYY-MM.md`).
fn rotation_marker_path(store: &Store) -> PathBuf {
    archive_dir(store).join(".rotating")
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

/// Byte offset of the first **valid** entry header — a `## [` line-start that
/// [`Log::parse_header`] accepts — or `None`.
///
/// Crucially this skips `## [`-SHAPED lines that `parse_header` REJECTS (a
/// merge-orphaned note, an exporter-malformed line) appearing before the first
/// real entry: everything up to the first valid header becomes the preserved
/// `header` block in [`parse_active`], so a rotation re-emits it verbatim.
/// Returning the first `## [`-shaped line instead (as this once did) put those
/// pre-entry lines into the entries region, where [`parse_entries`] — which
/// opens an entry only on a parseable header — dropped them on the floor,
/// silently erasing append-only content on the next rotation.
fn find_first_header(content: &str) -> Option<usize> {
    let mut offset = 0usize;
    for line in content.split_inclusive('\n') {
        let line_str = line.trim_end_matches(['\r', '\n']);
        if line_str.starts_with("## [") && Log::parse_header(line_str).is_some() {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

/// Whether `line` is a note line that — left unescaped — could be mistaken for
/// an entry header. It is *header-ambiguous* when it is a (possibly empty) run
/// of leading backslashes followed by a string that [`Log::parse_header`]
/// accepts. The escape (one leading backslash) and only the escape is added to,
/// or stripped from, such lines, so the transform is fully reversible:
/// `## [..]` (a real header shape in note text) ⇄ `\## [..]`, and a literal
/// `\## [..]` a note already contains ⇄ `\\## [..]`.
fn is_header_ambiguous(line: &str) -> bool {
    let stripped = line.trim_start_matches('\\');
    // Only treat it as ambiguous if some backslashes were the *only* prefix and
    // the remainder is a valid header — a backslash run that does not lead into
    // a header (e.g. `\not a header`) is ordinary note text, left untouched.
    Log::parse_header(stripped).is_some()
}

/// Escape one note line for on-disk emission so it can never be parsed as an
/// entry header (the [write-path fix] for header-shaped notes corrupting the
/// append-only log). A header-ambiguous line is prefixed with a single
/// backslash, moving its `## [` off column 0; every other line is emitted
/// verbatim. Reversed exactly by [`unescape_note_line`].
fn escape_note_line(line: &str) -> std::borrow::Cow<'_, str> {
    if is_header_ambiguous(line) {
        std::borrow::Cow::Owned(format!("\\{line}"))
    } else {
        std::borrow::Cow::Borrowed(line)
    }
}

/// Reverse [`escape_note_line`]: strip exactly one leading backslash from a
/// header-ambiguous on-disk note line, restoring the literal the author wrote.
/// A line that is not header-ambiguous (including a genuine `\not a header`) is
/// returned untouched, so the round-trip is lossless for arbitrary note text.
fn unescape_note_line(line: &str) -> std::borrow::Cow<'_, str> {
    if let Some(rest) = line.strip_prefix('\\') {
        if is_header_ambiguous(line) {
            return std::borrow::Cow::Borrowed(rest);
        }
    }
    std::borrow::Cow::Borrowed(line)
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
            // Reverse the per-line header escape `render` applies so an escaped
            // header-shaped note line round-trips back to its literal form.
            let joined = note
                .iter()
                .map(|line| unescape_note_line(line))
                .collect::<Vec<_>>()
                .join("\n");
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
///
/// **`recovering` — the re-roll gate.** Rotation in [`Log::append`] is two
/// non-atomic durable writes: roll prior-month entries into the archive, then
/// rewrite (trim) the active file. If the process crashes or the active rewrite
/// errors *after* the archive write commits, the prior-month entries remain in
/// the still-untrimmed active file and the agent's retry re-rolls them here. A
/// naive concatenate would then duplicate every entry, amplifying on each retry.
///
/// We CANNOT dedup that away by content alone: on-disk headers are
/// minute-precision, so two genuinely-distinct appends to the same object in the
/// same minute with the same note render byte-identically — indistinguishable
/// from a re-roll of one physical entry. Deduping unconditionally therefore
/// silently destroyed a legitimately-distinct backdated duplicate (the bug).
///
/// So the caller passes `recovering`: `true` only when an in-progress-rotation
/// marker was found (a crash-retry), where we dedup the incoming batch against
/// the archive **by multiplicity** (skip an incoming entry only while the
/// archive still holds an unconsumed copy of its identity) so a re-roll of the
/// SAME physical entries adds nothing. On a fresh rotation (`false`) every entry
/// is genuinely new to the archive and is appended unconditionally, so a
/// distinct same-minute repeat survives.
fn append_to_archive(path: &Path, entries: &[LogEntry], recovering: bool) -> crate::Result<()> {
    if path.exists() {
        let existing = fs::read_to_string(path)?;

        let mut body = String::new();
        if recovering {
            // Crash-retry: the prior (crashed) attempt may already have appended
            // some/all of these. Dedup by MULTIPLICITY, not set-membership, so a
            // partial-then-retried roll converges exactly and a re-roll of the
            // full batch is a no-op.
            let (_header, existing_entries) = parse_active(&existing);
            let mut remaining: std::collections::HashMap<EntryKey, usize> =
                std::collections::HashMap::new();
            for e in &existing_entries {
                *remaining.entry(entry_key(e)).or_insert(0) += 1;
            }
            for e in entries {
                match remaining.get_mut(&entry_key(e)) {
                    // An archived copy is still unconsumed: this incoming entry is
                    // that re-roll, suppress it.
                    Some(count) if *count > 0 => *count -= 1,
                    _ => body.push_str(&e.render()),
                }
            }
        } else {
            // Fresh rotation: append every entry. A same-minute, same-fields
            // entry that already exists in the archive is a DISTINCT append, not
            // a re-roll, and must be preserved.
            for e in entries {
                body.push_str(&e.render());
            }
        }

        // Nothing new to add (a fully-duplicate re-roll): leave the archive
        // byte-for-byte untouched (append-only: don't rewrite identical data).
        if body.is_empty() {
            return Ok(());
        }

        let mut full = existing;
        if !full.ends_with('\n') {
            full.push('\n');
        }
        full.push_str(&body);
        crate::fsx::write_atomic(path, full.as_bytes())?;
    } else {
        let mut body = String::new();
        for e in entries {
            body.push_str(&e.render());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let full = compose_active(LOG_FRONTMATTER, &body);
        crate::fsx::write_atomic(path, full.as_bytes())?;
    }
    Ok(())
}

/// A hashable identity for a log entry, used to dedup an idempotent archive
/// re-roll (see [`append_to_archive`]). Two entries are "the same" when their
/// timestamp, kind, object, and note all match — exactly the fields that
/// round-trip through `render`/`parse`, so a re-rolled entry compares equal to
/// the one already archived. Owned (rather than borrowed) so keys from the
/// existing archive and from the incoming entries share one type regardless of
/// where they came from; the cost is paid only on the cold rotation path.
type EntryKey = (DateTime<FixedOffset>, String, Option<String>, String);

/// Derive the dedup key for `e` (see [`EntryKey`]). Keying on `kind.as_str()`
/// (rather than `LogKind`, which is not `Hash`) is exact: `as_str`/`parse`
/// round-trips every recognized kind and preserves any `Custom` token.
fn entry_key(e: &LogEntry) -> EntryKey {
    (
        e.timestamp,
        e.kind.as_str().to_string(),
        e.object.clone(),
        e.note.clone(),
    )
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
    // The month must be a real calendar month. A hand-created / externally-
    // produced `log/2026-00.md` or `log/2026-13.md` parses as two digits but
    // names no month; returning `Some((year, 0))` would sort it below every
    // legitimate month, so the newest-month-first early-break in `since`/`tail`
    // could prune it and silently drop its entries. Out-of-range → `None`, so the
    // caller scans the file instead of risk-skipping it (the safe fallback).
    if !(1..=12).contains(&month) {
        return None;
    }
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
    // headers already handed to `take`, so re-visiting a header in a later block
    // never double-emits.
    let mut buf: Vec<u8> = Vec::new();
    let mut start = len;
    // O(1) membership: a `Vec` + `.contains()` here would be O(E²) across a large
    // single-month file (every header re-checked against all prior emissions).
    let mut emitted_abs: std::collections::HashSet<u64> = std::collections::HashSet::new();
    // Every header's absolute offset found so far, ascending. Built
    // *incrementally*: each block contributes only the markers whose `#` starts
    // inside it (all strictly smaller than any already-known offset, so they
    // prepend in order). This is the fix for the accidental O(file²) scan — the
    // old code re-ran `header_offsets` over the whole accumulated buffer on every
    // block (O(file²/block) byte comparisons on the default no-early-stop
    // tail/since path); now each byte is scanned for a header exactly once.
    let mut headers: Vec<u64> = Vec::new();
    let mut stop = false;
    // The first backward block has no already-scanned region to its right, so it
    // scans exactly `[0, block)`; every later block scans one byte further
    // (`block + 1`) to re-classify the prior block's deferred left-edge candidate
    // now that its left neighbour is buffered (see the scan call below).
    let mut first = true;

    while start > 0 && !stop {
        let block = std::cmp::min(REVERSE_BLOCK as u64, start);
        let new_start = start - block;
        file.seek(SeekFrom::Start(new_start))?;
        let mut chunk = vec![0u8; block as usize];
        file.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&buf);
        buf = chunk;
        start = new_start;

        // Scan the freshly-prepended block (buffer indices `[0, block)`) for new
        // header markers. A marker straddling the block boundary has its `#` in
        // this window and so is still caught (see `header_offsets_range`).
        //
        // One subtlety the scan must respect: a `## [` whose `#` sits at the
        // block's LEFT edge (buffer index 0, absolute offset `start`) cannot have
        // its line-start confirmed yet when `start > 0` — the byte at `start - 1`
        // is not buffered. Treating index 0 as a line start there fabricates an
        // entry from a mid-line `## [` fragment that happens to align with a block
        // boundary. So `header_offsets_range` DEFERS the leftmost candidate when
        // `base` is not the true file start, and we re-scan one byte further
        // right next time: after the first block the buffer carries the previous
        // block's left-edge byte at index `block` with its left neighbour now in
        // hand, so extending the window to `block + 1` re-classifies that exactly
        // once. `first` guards the first block (nothing to re-check on its right).
        let base_is_file_start = start == 0;
        let scan_hi = if first { block } else { block + 1 } as usize;
        let mut new_headers = header_offsets_range(&buf, start, 0, scan_hi, base_is_file_start);
        first = false;
        if !new_headers.is_empty() {
            new_headers.extend_from_slice(&headers);
            headers = new_headers;
        }

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
                emitted_abs.insert(abs);
                if take(entry) {
                    stop = true;
                    break;
                }
            } else {
                emitted_abs.insert(abs);
            }
        }
    }

    // Reached file start (or stopped). If we stopped, done. If we reached
    // start, emit any held-back oldest header(s) now (start == 0 means the
    // buffer's first header is genuinely the oldest). `headers` already holds
    // every offset (the loop scanned down to start == 0), so reuse it.
    if !stop && start == 0 {
        for i in (0..headers.len()).rev() {
            let abs = headers[i];
            if emitted_abs.contains(&abs) {
                continue;
            }
            let entry_text = entry_text_at(&buf, start, abs, &headers, i);
            if let Some(entry) = parse_single_entry(&entry_text) {
                emitted_abs.insert(abs);
                if take(entry) {
                    break;
                }
            } else {
                emitted_abs.insert(abs);
            }
        }
    }

    Ok(())
}

/// Absolute byte offsets of every **valid** entry-header line-start (`## […]`)
/// in `buf`, where `buf` begins at absolute offset `base`.
///
/// Only a `## [` line that [`Log::parse_header`] accepts is an entry boundary,
/// mirroring the forward parser ([`parse_entries`]), which folds an unparseable
/// `## [` line into the preceding entry's note rather than starting a new entry.
/// Without this validity check the reverse reader would split a real entry's
/// multi-line note at a continuation line beginning at column 0 with `## [`
/// (a shape the SPEC permits — notes are "one or more lines" with no
/// restriction), truncating the note and dropping the carved pseudo-entry, so
/// `tail`/`since`/`last_validate_at` would return a note diverging from the
/// intact on-disk bytes.
///
/// Whole-buffer convenience wrapper over [`header_offsets_range`]. The runtime
/// reverse reader now always scans incrementally (one freshly-prepended window
/// per backward block), so this whole-buffer form is retained only as the
/// oracle the range-scan tests check the incremental scan against.
#[cfg(test)]
fn header_offsets(buf: &[u8], base: u64) -> Vec<u64> {
    // The whole-buffer oracle treats `base` as the file start iff it is 0, so a
    // `## [` at buffer index 0 is a real line-start there.
    header_offsets_range(buf, base, 0, buf.len(), base == 0)
}

/// Like [`header_offsets`] but only reports header *markers whose `#` starts in*
/// `buf[scan_lo..scan_hi)`, while still consulting bytes outside that window —
/// to the left for the line-start (`buf[i-1] == b'\n'`) check and to the right
/// for the header line's content. This is the incremental scan
/// [`reverse_collect`] uses: each backward block searches only the freshly-
/// prepended region for *new* markers, so total header-scan work is linear in
/// the file size, not the O(file²) of re-scanning the whole growing buffer on
/// every block.
///
/// A `## [` marker that *straddles* the boundary (its `#` in the new block, its
/// `[` or trailing bytes in the already-scanned region) is still detected here:
/// its `#` index is `< scan_hi`, so it falls in this window, and it was never
/// reported by an earlier scan (whose window was `[block, …)`, strictly to the
/// right of this one) — so each marker is reported exactly once across all
/// blocks.
///
/// **Left-edge line-start safety.** A `## [` whose `#` is at buffer index 0 has
/// no buffered left neighbour, so its line-start cannot be confirmed unless
/// index 0 really is the file start. `base_is_file_start` says so: when it is
/// `false`, an index-0 candidate is DEFERRED (not reported) rather than assumed
/// to be at a line start — otherwise a mid-line `## […]` fragment that happens
/// to align with a block's left edge would be fabricated into an entry,
/// truncating the real entry's note and (after rotation) corrupting the
/// append-only archive. The caller re-scans that byte on the next block, once
/// its left neighbour is buffered, so a genuine boundary header is still found
/// exactly once.
fn header_offsets_range(
    buf: &[u8],
    base: u64,
    scan_lo: usize,
    scan_hi: usize,
    base_is_file_start: bool,
) -> Vec<u64> {
    const PAT: &[u8] = b"## [";
    let mut out = Vec::new();
    let n = buf.len();
    let hi = scan_hi.min(n);
    let mut i = scan_lo;
    // A marker's `#` must start strictly before `hi`; the pattern/line content
    // may read past `hi` into `buf` (the right neighbour is already buffered).
    while i < hi && i + PAT.len() <= n {
        if &buf[i..i + PAT.len()] == PAT {
            // Index 0 is a line start only when it is the genuine file start;
            // otherwise its left neighbour is unbuffered and the candidate is
            // deferred to the next block (see the doc comment).
            let at_line_start = if i == 0 {
                base_is_file_start
            } else {
                buf[i - 1] == b'\n'
            };
            if at_line_start && is_valid_header_line(buf, i) {
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

/// Whether the `## [` line starting at byte `i` in `buf` parses as a valid
/// entry header. Reads the line up to (but not including) the next `\n` (or
/// buffer end) and defers to [`Log::parse_header`] — the same validity gate the
/// forward parser applies, keeping the reverse reader's boundary set identical
/// to the forward one.
fn is_valid_header_line(buf: &[u8], i: usize) -> bool {
    let line_end = buf[i..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|p| i + p)
        .unwrap_or(buf.len());
    let line = String::from_utf8_lossy(&buf[i..line_end]);
    Log::parse_header(&line).is_some()
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

    /// Regression (adversarial review): a hand-created / externally-produced
    /// archive with an out-of-range month (`00`, `13`..`99`) must NOT parse as a
    /// real month archive — otherwise its `(year, 0)` bucket sorts below every
    /// legitimate month and the newest-first early-break in `since`/`tail` can
    /// silently prune it. Out-of-range → `None` (the caller scans it instead).
    #[test]
    fn archive_year_month_rejects_out_of_range_months() {
        use std::path::Path;
        assert_eq!(
            archive_year_month(Path::new("log/2026-05.md")),
            Some((2026, 5))
        );
        assert_eq!(
            archive_year_month(Path::new("log/2026-01.md")),
            Some((2026, 1))
        );
        assert_eq!(
            archive_year_month(Path::new("log/2026-12.md")),
            Some((2026, 12))
        );
        for bad in ["log/2026-00.md", "log/2026-13.md", "log/2026-99.md"] {
            assert_eq!(
                archive_year_month(Path::new(bad)),
                None,
                "{bad} has an out-of-range month and must not parse as an archive"
            );
        }
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

    // ── regression: rotation re-roll must not duplicate archive entries (#3) ──

    /// Count occurrences of `needle` in `haystack` (non-overlapping).
    fn count_occurrences(haystack: &str, needle: &str) -> usize {
        haystack.matches(needle).count()
    }

    #[test]
    fn regression_archive_reroll_is_idempotent_after_interrupted_rotation() {
        // Reconstructs the finding's exact failure window: rotation is two
        // non-atomic durable writes — (1) roll prior-month entries into the
        // archive, then (2) trim the active file. If the process crashes or the
        // active rewrite errors AFTER step (1) commits, the prior-month entries
        // stay in the untrimmed active file, the agent retries, and the retry
        // re-rolls the SAME entries into the archive a second time. The
        // mechanism is precisely a second `append_to_archive` of identical
        // entries onto an archive that already holds them.
        let (_d, store) = temp_store();
        let dir = archive_dir(&store);
        let arch = archive_path(&store, 2026, 4);

        let apr1 = entry(2026, 4, 10, 9, 0, LogKind::Ingest, Some("apr-a"), "apr one");
        let apr2 = entry(2026, 4, 20, 9, 0, LogKind::Create, Some("apr-b"), "apr two");
        let month = [apr1.clone(), apr2.clone()];

        // First roll: a FRESH rotation (no in-progress marker) appends both.
        fs::create_dir_all(&dir).unwrap();
        append_to_archive(&arch, &month, false).unwrap();

        // The retries are crash-RECOVERIES (the in-progress-rotation marker is
        // present), so they dedup the re-rolled identical entries to a no-op.
        // Pre-fix this blindly concatenated, doubling every entry; do it twice to
        // prove the amplification a real retry loop would cause is suppressed.
        append_to_archive(&arch, &month, true).unwrap();
        append_to_archive(&arch, &month, true).unwrap();

        let archived = fs::read_to_string(&arch).unwrap();
        // Each entry header must appear EXACTLY once despite the re-rolls.
        assert_eq!(
            count_occurrences(&archived, "## [2026-04-10 09:00] ingest | apr-a"),
            1,
            "re-rolled archive duplicated the first April entry; got:\n{archived}"
        );
        assert_eq!(
            count_occurrences(&archived, "## [2026-04-20 09:00] create | apr-b"),
            1,
            "re-rolled archive duplicated the second April entry; got:\n{archived}"
        );

        // And the reader surface (`since`) must return each entry once, not the
        // duplicated set the pre-fix archive would have yielded.
        let got = Log::since(&store, ts(2026, 4, 1, 0, 0)).unwrap();
        assert_eq!(
            got,
            vec![apr1, apr2],
            "since over the re-rolled archive must return each April entry once"
        );
    }

    #[test]
    fn regression_rotation_reroll_after_active_untrimmed_does_not_duplicate() {
        // End-to-end variant driving the real `Log::append` rotation path. We
        // rotate April into its archive via a May append, then SIMULATE the
        // partial failure by restoring the pre-trim active file (April + May)
        // and re-running `append` — exactly the state a crash-between-the-two-
        // writes / failed-active-rewrite + agent-retry produces. The archive
        // must still hold each April entry once.
        let (_d, store) = temp_store();
        let apr1 = entry(2026, 4, 10, 9, 0, LogKind::Ingest, Some("apr-a"), "apr one");
        let apr2 = entry(2026, 4, 20, 9, 0, LogKind::Create, Some("apr-b"), "apr two");
        Log::append(&store, &apr1).unwrap();
        Log::append(&store, &apr2).unwrap();

        // Snapshot the active file holding both April entries (this is what is
        // still on disk if the post-rotation active rewrite never lands).
        let active_path = active_log_path(&store);
        let pre_rotation_active = fs::read_to_string(&active_path).unwrap();

        // A May append rotates April out and trims the active file.
        let may = entry(2026, 5, 2, 8, 0, LogKind::Update, Some("may-a"), "may one");
        Log::append(&store, &may).unwrap();
        let arch = archive_path(&store, 2026, 4);
        assert!(arch.exists(), "April should have rotated to its archive");

        // Simulate the crash/error: the active rewrite never persisted, so the
        // active file still contains the (now also archived) April entries.
        fs::write(&active_path, &pre_rotation_active).unwrap();
        // A real crash leaves the in-progress-rotation marker behind too — it is
        // deleted only AFTER the active trim commits. Restore it so the retry is
        // recognized as a crash-recovery re-roll (deduped), not a fresh rotation
        // (which would correctly append a genuinely-distinct repeat).
        fs::write(rotation_marker_path(&store), b"").unwrap();

        // The agent retries the append. Re-partitioning sees April as prior
        // months again and re-rolls them — which must NOT duplicate the archive.
        let may2 = entry(2026, 5, 3, 8, 0, LogKind::Update, Some("may-b"), "may two");
        Log::append(&store, &may2).unwrap();

        let archived = fs::read_to_string(&arch).unwrap();
        assert_eq!(
            count_occurrences(&archived, "## [2026-04-10 09:00] ingest | apr-a"),
            1,
            "retried rotation duplicated an April entry in the archive; got:\n{archived}"
        );
        assert_eq!(
            count_occurrences(&archived, "## [2026-04-20 09:00] create | apr-b"),
            1,
            "retried rotation duplicated an April entry in the archive; got:\n{archived}"
        );
    }

    /// Adversarial review (#7) — two GENUINELY-DISTINCT appends that render
    /// byte-identically at minute precision (same minute/kind/object/note) must
    /// BOTH survive rotation. The backdated-duplicate case: apr1 rotates in May;
    /// the backdated apr2 lands in the active file later and rotates in June as a
    /// FRESH roll (no in-progress marker), so it must be appended even though the
    /// April archive already holds the byte-identical apr1. Pre-fix the
    /// set-membership dedup dropped apr2 — silent, unrecoverable audit-log loss.
    #[test]
    fn regression_distinct_same_minute_entries_both_survive_rotation() {
        let (_d, store) = temp_store();
        let apr1 = entry(2026, 4, 10, 9, 0, LogKind::Ingest, Some("x"), "dup");
        let apr2 = entry(2026, 4, 10, 9, 0, LogKind::Ingest, Some("x"), "dup");

        Log::append(&store, &apr1).unwrap();
        // A May append rotates apr1 into the April archive and COMPLETES (no
        // marker left behind).
        Log::append(
            &store,
            &entry(2026, 5, 2, 8, 0, LogKind::Ingest, Some("may"), "m"),
        )
        .unwrap();
        // The backdated apr2 lands in the active file beside the May entry.
        Log::append(&store, &apr2).unwrap();
        // A June append rotates the May entry AND apr2 out. apr2 is a fresh roll.
        Log::append(
            &store,
            &entry(2026, 6, 1, 8, 0, LogKind::Ingest, Some("jun"), "j"),
        )
        .unwrap();

        let archived = fs::read_to_string(archive_path(&store, 2026, 4)).unwrap();
        assert_eq!(
            count_occurrences(&archived, "## [2026-04-10 09:00] ingest | x"),
            2,
            "two distinct same-minute April appends must BOTH survive rotation; got:\n{archived}"
        );
        // The reader must return both too (read-dedup must not collapse distinct
        // same-minute archive entries).
        let got = Log::since(&store, ts(2026, 4, 1, 0, 0)).unwrap();
        let dups = got
            .iter()
            .filter(|e| e.object.as_deref() == Some("x"))
            .count();
        assert_eq!(
            dups, 2,
            "since must return both distinct same-minute entries; got {got:#?}"
        );
    }

    /// Adversarial review (#12) — `tail`/`since` must return two byte-identical
    /// same-minute entries that both live in the ACTIVE log (no archive). Pre-fix
    /// a global content-keyed `seen` set suppressed the second on read, so the
    /// reader under-reported what was on disk (`grep` saw 2, `tail` saw 1).
    #[test]
    fn regression_tail_since_return_distinct_same_minute_active_entries() {
        let (_d, store) = temp_store();
        Log::append(
            &store,
            &entry(2026, 6, 10, 9, 0, LogKind::Ingest, Some("x"), "dup"),
        )
        .unwrap();
        Log::append(
            &store,
            &entry(2026, 6, 10, 9, 0, LogKind::Ingest, Some("x"), "dup"),
        )
        .unwrap();

        let tail = Log::tail(&store, 20).unwrap();
        assert_eq!(
            tail.len(),
            2,
            "tail must return both same-minute active entries; got {tail:#?}"
        );
        let since = Log::since(&store, ts(2026, 6, 1, 0, 0)).unwrap();
        assert_eq!(
            since.len(),
            2,
            "since must return both same-minute active entries; got {since:#?}"
        );
    }

    /// Adversarial review (#15) — rotation must NOT erase lines before the first
    /// VALID entry header. An active log whose entries region opens with a
    /// `## [`-shaped line that `parse_header` rejects (a merge orphan / malformed
    /// export) before the first real entry: pre-fix `find_first_header` landed on
    /// it, `parse_entries` dropped it (no open entry yet), and the rotation
    /// re-emitted without it — silently erasing append-only content. The fix
    /// folds everything before the first valid header into the preserved header
    /// block, which rotation re-emits verbatim.
    #[test]
    fn regression_rotation_preserves_lines_before_first_valid_header() {
        let (_d, store) = temp_store();
        let active = active_log_path(&store);
        let content = "---\ntype: log\n---\n\n## [orphan from a merge] stray text\n## [2026-04-10 09:00] ingest | x\nbody line\n";
        fs::write(&active, content).unwrap();

        // A June append rotates the April entry out and rewrites the active file.
        Log::append(
            &store,
            &entry(2026, 6, 1, 8, 0, LogKind::Ingest, Some("jun"), "j"),
        )
        .unwrap();

        let active_after = fs::read_to_string(&active).unwrap();
        let arch_after = fs::read_to_string(archive_path(&store, 2026, 4)).unwrap_or_default();
        assert!(
            active_after.contains("orphan from a merge") || arch_after.contains("orphan from a merge"),
            "the pre-first-valid-header line was erased by rotation;\nactive:\n{active_after}\narchive:\n{arch_after}"
        );
        // Sanity: the real April entry still rotated into its archive.
        assert!(
            arch_after.contains("## [2026-04-10 09:00] ingest | x"),
            "the valid April entry must still rotate to its archive; got:\n{arch_after}"
        );
    }

    // ── regression: reverse reader keeps a `## [` continuation note line (#10) ─

    #[test]
    fn regression_reverse_reader_preserves_note_line_starting_with_bracket_header() {
        // SPEC permits a note of "one or more lines" with no restriction on a
        // continuation line starting at column 0 with `## [`. The forward parser
        // folds such an unparseable `## [` line into the note; the reverse
        // reader (tail/since/last_validate_at) must agree, not split on it.
        let (_d, store) = temp_store();
        let multi = "First line.\n## [draft outline] more\nThird line.";
        let e = entry(
            2026,
            5,
            27,
            10,
            0,
            LogKind::Update,
            Some("records/x"),
            multi,
        );
        // Author the log verbatim (render writes the note as-is); this is the
        // on-disk shape a hand-written / appended multi-line note produces.
        write_raw_log(&store, std::slice::from_ref(&e));

        // Pre-fix: header_offsets treated `## [draft outline] more` as a second
        // entry boundary, truncating the note to "First line." and dropping the
        // carved (non-header) fragment. Post-fix: the full note survives.
        let got = Log::tail(&store, 1).unwrap();
        assert_eq!(got.len(), 1, "the single entry must be returned");
        assert_eq!(
            got[0].note, multi,
            "reverse reader truncated the note at the `## [` continuation line; \
             got {:?}",
            got[0].note
        );
        assert_eq!(got[0], e, "the whole entry must round-trip through tail");

        // `since` (the other reverse-reading surface) must agree.
        let since = Log::since(&store, ts(2026, 5, 27, 9, 0)).unwrap();
        assert_eq!(since, vec![e]);
    }

    // ── regression: `since` archive pruning uses the UTC month, not local (#11) ─

    /// A `DateTime<FixedOffset>` at the given fixed offset (hours east of UTC).
    fn ts_offset(
        y: i32,
        mo: u32,
        d: u32,
        h: u32,
        mi: u32,
        offset_hours: i32,
    ) -> DateTime<FixedOffset> {
        let naive = chrono::NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap();
        FixedOffset::east_opt(offset_hours * 3600)
            .unwrap()
            .from_local_datetime(&naive)
            .single()
            .unwrap()
    }

    #[test]
    fn regression_since_prunes_archives_on_utc_month_not_local_offset_month() {
        // Archive months are bucketed on the UTC calendar. A `since` cutoff with
        // a non-UTC offset near a month boundary must not prune an archive whose
        // UTC month equals the cutoff's UTC month just because the cutoff's
        // LOCAL month is later.
        let (_d, store) = temp_store();

        // April archive: an entry late on 2026-04-30 at 18:00 UTC.
        let apr = entry(
            2026,
            4,
            30,
            18,
            0,
            LogKind::Update,
            Some("apr-late"),
            "april late",
        );
        let dir = archive_dir(&store);
        fs::create_dir_all(&dir).unwrap();
        let mut arch = String::from(LOG_FRONTMATTER);
        arch.push('\n');
        arch.push_str(&apr.render());
        fs::write(archive_path(&store, 2026, 4), arch).unwrap();

        // Active file: a clean May entry, so an archive scan is actually needed.
        let may = entry(2026, 5, 5, 8, 0, LogKind::Update, Some("may-a"), "may one");
        write_raw_log(&store, std::slice::from_ref(&may));

        // Cutoff 2026-05-01T00:30:00+07:00 == 2026-04-30T17:30:00Z. The April
        // 18:00 UTC entry is strictly newer than this instant.
        let cutoff = ts_offset(2026, 5, 1, 0, 30, 7);
        // Sanity: the cutoff's UTC month is April, its local month is May.
        assert_eq!((cutoff.year(), cutoff.month()), (2026, 5));
        assert_eq!(
            (
                cutoff.with_timezone(&Utc).year(),
                cutoff.with_timezone(&Utc).month()
            ),
            (2026, 4)
        );

        // Pre-fix: cutoff_ym = (2026, 5) from local fields, so the (2026, 4)
        // archive was pruned and the genuinely-newer 18:00 UTC entry was dropped
        // — `since` returned only the May entry. Post-fix: cutoff_ym is UTC
        // (2026, 4), the April archive is scanned, and both come back.
        let got = Log::since(&store, cutoff).unwrap();
        let stamps: std::collections::BTreeSet<_> = got.iter().map(|e| e.timestamp).collect();
        assert_eq!(
            stamps,
            [ts(2026, 4, 30, 18, 0), ts(2026, 5, 5, 8, 0)]
                .into_iter()
                .collect(),
            "since(non-UTC cutoff near a month boundary) must include the April \
             archive entry newer than the cutoff instant; got {got:?}"
        );
    }

    // ── regression: header-shaped note line corrupts the append-only log (#critical)

    #[test]
    fn note_line_shaped_like_a_header_is_escaped_and_round_trips() {
        // A `contradiction` note quoting an earlier entry header is the
        // demonstrated corruption: the verbatim `## [2020-01-01 00:00] delete |
        // …` line was parsed as a REAL entry on readback (fabricated entry, real
        // note truncated). With write-path escaping it stays note body.
        let (_d, store) = temp_store();
        let note = "quoting earlier entry:\n## [2020-01-01 00:00] delete | records/contacts/jane.md\nend of quote";
        let e = entry(
            2026,
            6,
            11,
            4,
            41,
            LogKind::Contradiction,
            Some("records/contacts/jane.md"),
            note,
        );
        Log::append(&store, &e).unwrap();

        // On disk: the header-shaped note line must NOT sit at column 0 as a
        // `## [` header — `grep "^## \["` must see exactly the one real header.
        let raw = fs::read_to_string(store.root.join("log.md")).unwrap();
        let header_lines = raw.lines().filter(|l| l.starts_with("## [")).count();
        assert_eq!(
            header_lines, 1,
            "exactly one real entry header may sit at column 0; got:\n{raw}"
        );

        // Readback returns ONE entry, with the full note intact (no fabricated
        // 2020 entry, no truncation).
        let got = Log::tail(&store, 10).unwrap();
        assert_eq!(got.len(), 1, "exactly one entry; got {got:?}");
        assert_eq!(got[0].note, note, "note must round-trip verbatim");
        assert_eq!(got[0], e);
        let since = Log::since(&store, ts(2026, 1, 1, 0, 0)).unwrap();
        assert_eq!(since, vec![e.clone()]);
    }

    #[test]
    fn header_shaped_note_survives_a_later_rotation_uncorrupted() {
        // Physical corruption: pre-fix, the fabricated past-dated pseudo-entry
        // (year 2020 < current) was rolled into an archive on the NEXT append,
        // splitting the real note. With escaping the line is note text, so a
        // later append never sees a phantom prior-month entry to roll out.
        let (_d, store) = temp_store();
        let note = "see\n## [2020-01-01 00:00] delete | records/x.md\nbelow";
        let first = entry(
            2026,
            6,
            11,
            4,
            41,
            LogKind::Contradiction,
            Some("records/x.md"),
            note,
        );
        Log::append(&store, &first).unwrap();

        // Append another current-month entry — the path that re-parses + may
        // rotate. No 2020 archive must be created and the first note stays whole.
        let second = entry(
            2026,
            6,
            11,
            5,
            0,
            LogKind::Update,
            Some("records/y.md"),
            "y",
        );
        Log::append(&store, &second).unwrap();

        assert!(
            !store.root.join("log").join("2020-01.md").exists(),
            "a header-shaped note line must not fabricate a 2020 archive"
        );
        let got = Log::tail(&store, 10).unwrap();
        assert_eq!(got.len(), 2, "two real entries only; got {got:?}");
        let first_back = got
            .iter()
            .find(|e| e.object.as_deref() == Some("records/x.md"));
        assert_eq!(
            first_back.map(|e| e.note.as_str()),
            Some(note),
            "the header-shaped note must survive the rotation pass intact"
        );
    }

    #[test]
    fn escape_unescape_note_line_round_trips_including_literal_backslash() {
        // The escape must be lossless for arbitrary note lines, including a line
        // the author genuinely wrote starting with `\` before a header shape.
        let valid_header = "## [2020-01-01 00:00] delete | x";
        // A real header shape: escaped on write, restored on read.
        assert_eq!(
            &*escape_note_line(valid_header),
            &format!("\\{valid_header}")
        );
        let escaped = escape_note_line(valid_header).into_owned();
        assert_eq!(&*unescape_note_line(&escaped), valid_header);
        // An already-`\`-prefixed header-shape line escapes to two backslashes
        // and restores to one (never collapses to a bare header).
        let pre = format!("\\{valid_header}");
        assert_eq!(&*escape_note_line(&pre), &format!("\\{pre}"));
        let pre_escaped = escape_note_line(&pre).into_owned();
        assert_eq!(&*unescape_note_line(&pre_escaped), &pre);
        // Ordinary text (including a `\` that does NOT lead into a header) is
        // untouched both ways.
        for plain in ["plain note", "## [not a header]", "\\not a header", ""] {
            assert_eq!(&*escape_note_line(plain), plain);
            assert_eq!(&*unescape_note_line(plain), plain);
        }
    }

    // ── regression: reverse reader scans each block once (no O(file²)) (#perf) ──

    #[test]
    fn reverse_read_correct_with_header_straddling_a_block_boundary() {
        // The incremental per-block header scan must still catch a `## [` marker
        // whose `#` falls in one block but whose bytes extend into the already-
        // scanned region. Build a log whose total size crosses several blocks and
        // verify a full read reconstructs every entry — the straddle case is hit
        // by construction across the many block boundaries.
        let (_d, store) = temp_store();
        let n = 600usize;
        let mut expected: Vec<LogEntry> = Vec::new();
        for i in 0..n {
            let total_min = (i as u32) * 2;
            let day = 1 + total_min / (24 * 60);
            let hour = (total_min / 60) % 24;
            let min = total_min % 60;
            // Vary note length so headers land at many offsets relative to the
            // fixed 8 KiB block grid, exercising boundary straddles.
            let note = format!("note {i} {}", "y".repeat(i % 97));
            let e = entry(
                2026,
                6,
                day,
                hour,
                min,
                LogKind::Update,
                Some(&format!("records/item-{i:05}")),
                &note,
            );
            Log::append(&store, &e).unwrap();
            expected.push(e);
        }
        let size = fs::metadata(store.root.join("log.md")).unwrap().len();
        assert!(
            size > (REVERSE_BLOCK as u64) * 3,
            "test log not large enough ({size} bytes) to cross several blocks"
        );
        let all = Log::tail(&store, n + 10).unwrap();
        assert_eq!(all, expected, "every entry must reconstruct across blocks");
        // A small tail must also be exact (the n-newest by timestamp).
        assert_eq!(Log::tail(&store, 7).unwrap(), expected[n - 7..].to_vec());
    }

    #[test]
    fn header_offsets_range_finds_boundary_straddling_marker_once() {
        // Two headers; `header_offsets` (whole-buffer) finds both. The range
        // scan with a window that splits the buffer between them must report the
        // one in its window exactly once, consulting the left neighbour for the
        // line-start check.
        let buf =
            b"## [2026-06-01 00:00] update | a\nnote a\n## [2026-06-01 00:01] update | b\nnote b\n";
        let full = header_offsets(buf, 0);
        assert_eq!(full.len(), 2, "both headers found over the whole buffer");
        let second = full[1] as usize;
        // A window covering only the SECOND header's `#` reports just it. Its `#`
        // is not at index 0, so `base_is_file_start` is irrelevant here.
        let only_second = header_offsets_range(buf, 0, second, second + 1, false);
        assert_eq!(only_second, vec![full[1]]);
        // A window covering only the FIRST reports just it (right content read
        // past the window into the buffer). `base == 0` is the true file start,
        // so the index-0 candidate is a real line start.
        let only_first = header_offsets_range(buf, 0, 0, 1, true);
        assert_eq!(only_first, vec![full[0]]);
        // Disjoint windows partition the markers with no double-count.
        let mut combined = header_offsets_range(buf, 0, 0, second, true);
        combined.extend(header_offsets_range(buf, 0, second, buf.len(), false));
        assert_eq!(combined, full);
    }

    /// CRITICAL regression: a MID-LINE `## [<valid header>]` fragment inside a
    /// real entry's note that happens to align with a reverse-read block boundary
    /// must NOT be fabricated into an entry. The incremental backward scan reads
    /// each block's left edge before its left neighbour is buffered; treating
    /// buffer index 0 as a line start there would carve a phantom entry from the
    /// fragment and truncate the real entry's note. The fix defers the left-edge
    /// candidate until its neighbour is read, so the fragment is correctly seen
    /// as note body (its `#` is not at a line start).
    #[test]
    fn reverse_read_does_not_fabricate_entry_from_midline_header_at_block_boundary() {
        let (_d, store) = temp_store();

        // A single real entry. Its note carries a mid-line `## [` fragment that
        // is a *valid* header shape but is NOT at column 0 (so the writer's
        // column-0 escape correctly leaves it verbatim — it is the trigger).
        let fragment = "see ## [2020-01-01 00:00] delete | records/x.md";
        let hash_in_fragment = fragment.find("##").expect("fragment has `##`");

        // Build the raw active log by hand so the fragment's `#` lands at the
        // FIRST backward block's left edge: the reverse reader anchors its blocks
        // at EOF (`new_start = len - REVERSE_BLOCK` on the first block), so the
        // `#` must sit exactly `REVERSE_BLOCK` bytes before EOF. We append note
        // padding AFTER the fragment to push EOF out to that distance.
        //
        // Layout (one entry):
        //   <frontmatter>\n## [<header>] | records/real.md\nlead\n<fragment><tail>\n\n
        let header_line = "## [2026-06-14 10:00] update | records/real.md\n";
        let mut head = String::from(LOG_FRONTMATTER);
        head.push('\n');
        head.push_str(header_line);
        head.push_str("lead\n");
        head.push_str(fragment); // fragment opens the second note line

        // Absolute offset of the fragment's `#`.
        let hash_off = head.len() - fragment.len() + hash_in_fragment;
        // We append `<tail>\n\n`. Bytes after `#` = (head.len() - hash_off) +
        // tail_len + 2. Need that == REVERSE_BLOCK so `#` is at `len -
        // REVERSE_BLOCK` (the first block's left edge).
        let after_hash_in_head = head.len() - hash_off;
        let tail_len = REVERSE_BLOCK
            .checked_sub(after_hash_in_head + 2)
            .expect("REVERSE_BLOCK comfortably exceeds the post-`#` head bytes");
        let mut body = head;
        body.push_str(&"z".repeat(tail_len)); // valid note bytes on the fragment line
        body.push('\n');
        body.push('\n');
        fs::write(store.root.join("log.md"), &body).unwrap();

        // The file must be large enough to cross at least one block boundary.
        assert!(
            body.len() as u64 > REVERSE_BLOCK as u64,
            "test log must span >1 block (len {})",
            body.len()
        );
        // And the fragment's `#` sits exactly at the first block's left edge.
        let real_hash_off = body.find("see ##").unwrap() + hash_in_fragment;
        assert_eq!(
            real_hash_off,
            body.len() - REVERSE_BLOCK,
            "fragment `#` must land on the first backward block's left edge to exercise the bug"
        );

        // Reverse read must return EXACTLY ONE entry — the real one — and never a
        // fabricated `2020-01-01 delete records/x.md` carved from the fragment.
        let got = Log::tail(&store, 10).unwrap();
        assert_eq!(
            got.len(),
            1,
            "exactly the one real entry; got {} (a fabricated entry means the boundary `#` was mis-read as a header): {got:#?}",
            got.len()
        );
        let only = &got[0];
        assert_eq!(only.object.as_deref(), Some("records/real.md"));
        assert_eq!(only.timestamp, ts(2026, 6, 14, 10, 0));
        // The note is intact end-to-end (not truncated at the fragment): both the
        // lead and the verbatim fragment survive.
        assert!(
            only.note.contains("lead"),
            "note keeps its lead; got {:?}",
            only.note
        );
        assert!(
            only.note.contains(fragment),
            "note keeps the verbatim mid-line fragment (not truncated); got {:?}",
            only.note
        );
    }

    // ── regression: tail/since dedup across active+archive on interrupted rotation

    #[test]
    fn tail_and_since_dedup_entries_present_in_both_active_and_archive() {
        // Reconstructs the finding's crash window: the archive write committed
        // but the active rewrite never trimmed, so the same April entries live in
        // BOTH the untrimmed active file and `log/2026-04.md`. Readers must
        // return each entry ONCE, not twice.
        let (_d, store) = temp_store();
        let apr_a = entry(2026, 4, 10, 9, 0, LogKind::Ingest, Some("apr-a"), "apr one");
        let apr_b = entry(2026, 4, 20, 9, 0, LogKind::Create, Some("apr-b"), "apr two");

        // Active file still holds both April entries (the un-trimmed state).
        write_raw_log(&store, &[apr_a.clone(), apr_b.clone()]);
        // The committed step-1 archive holds the same two entries.
        let dir = archive_dir(&store);
        fs::create_dir_all(&dir).unwrap();
        let mut arch = String::from(LOG_FRONTMATTER);
        arch.push('\n');
        arch.push_str(&apr_a.render());
        arch.push_str(&apr_b.render());
        fs::write(archive_path(&store, 2026, 4), arch).unwrap();

        // `since` must return each April entry exactly once.
        let since = Log::since(&store, ts(2026, 4, 1, 0, 0)).unwrap();
        assert_eq!(
            since,
            vec![apr_a.clone(), apr_b.clone()],
            "since must dedup the doubly-present entries; got {since:?}"
        );

        // `tail` must too — no duplicate window slots.
        let tail = Log::tail(&store, 10).unwrap();
        assert_eq!(
            tail,
            vec![apr_a, apr_b],
            "tail must dedup the doubly-present entries; got {tail:?}"
        );
    }
}
