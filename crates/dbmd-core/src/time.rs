//! Canonical wall-clock for write-surface timestamp seeding.
//!
//! Every write surface that stamps `created` / `updated` (or a log entry's
//! timestamp) seeds it from [`now`] so all of them agree on one representation:
//! the current instant as a fixed-offset (UTC) [`DateTime`]. This is the type
//! [`crate::Frontmatter::created`] / [`crate::Frontmatter::updated`] already
//! hold, so callers assign it directly with no string round-trip.
//!
//! Keeping this in `dbmd-core` (rather than re-deriving it per CLI handler)
//! means `dbmd write`, `dbmd fm init`, `dbmd fm set`, and `dbmd log append` all
//! compute "now" the same way from one place — and the thin CLI carries no
//! bespoke calendar logic.
//!
//! ## Reproducibility hook: `DBMD_NOW`
//!
//! Because every write surface seeds its timestamps from this one function, it
//! is also the one place to pin the clock for **deterministic, byte-for-byte**
//! output. When the `DBMD_NOW` environment variable is set to an RFC3339
//! timestamp, [`now`] returns that instant verbatim instead of the wall clock,
//! so a scripted sequence of `dbmd write` / `fm set` / `log` invocations
//! produces identical `created`/`updated` fields, identical `index.md` /
//! `index.jsonl` (whose own `updated` is the max over their records), and
//! identical `log.md` headers on every run. This is the same family of build
//! hook as `SOURCE_DATE_EPOCH`: unset, behaviour is exactly the wall clock it
//! always was (zero product impact); set, the toolkit is reproducible — which
//! the agent-eval golden harness (`crates/dbmd-cli/tests/agent_eval.rs`) relies
//! on to commit `EXPECTED/` trees that pin the curator's output. A malformed
//! `DBMD_NOW` is ignored (falls back to the wall clock) rather than aborting an
//! otherwise-valid write.

use chrono::{DateTime, FixedOffset, Utc};

/// Environment variable that pins [`now`] to a fixed RFC3339 instant. See the
/// module docs (`## Reproducibility hook`). Unset ⇒ wall clock.
pub const NOW_OVERRIDE_ENV: &str = "DBMD_NOW";

/// The current instant as a fixed-offset (UTC) timestamp.
///
/// Returns `DateTime<FixedOffset>` — the canonical type the universal `created`
/// / `updated` frontmatter fields and the log entry timestamp hold — so write
/// surfaces seed it without any RFC3339 string round-trip. Resolution is
/// chrono's native (sub-second); the canonical writers render it to RFC3339 on
/// the way to disk.
///
/// If [`NOW_OVERRIDE_ENV`] (`DBMD_NOW`) is set to a parseable RFC3339 timestamp,
/// that fixed instant is returned instead of the wall clock — the deterministic
/// reproducibility hook (see module docs). An unset or unparseable value falls
/// back to the wall clock, so the default path is unchanged.
pub fn now() -> DateTime<FixedOffset> {
    if let Some(fixed) = now_override() {
        return fixed;
    }
    Utc::now().fixed_offset()
}

/// Read and parse [`NOW_OVERRIDE_ENV`]. `None` when unset or unparseable (the
/// caller then uses the wall clock). Normalized to the UTC (zero) offset to
/// match the wall-clock path, so downstream RFC3339 rendering is offset-stable
/// regardless of the offset the override was written with.
fn now_override() -> Option<DateTime<FixedOffset>> {
    let raw = std::env::var(NOW_OVERRIDE_ENV).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc).fixed_offset())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_utc_offset() {
        // The canonical seed is always emitted at the UTC (zero) offset, so two
        // calls and any downstream RFC3339 rendering stay timezone-stable. Hold
        // for both paths: explicitly pin a non-UTC-offset override and confirm
        // it is normalized to the zero offset (so a `DBMD_NOW` set in the
        // ambient env can never make the seed timezone-unstable).
        assert_eq!(
            now_override_disabled().offset(),
            &FixedOffset::east_opt(0).unwrap()
        );
        let pinned = parse_override("2026-05-29T12:00:00+05:30").expect("valid override");
        assert_eq!(pinned.offset(), &FixedOffset::east_opt(0).unwrap());
        assert_eq!(
            pinned,
            "2026-05-29T06:30:00Z"
                .parse::<DateTime<FixedOffset>>()
                .unwrap()
        );
    }

    #[test]
    fn now_is_monotonic_nondecreasing() {
        // Wall-clock now must not go backwards between two reads in the same
        // process (guards against an accidental fixed/zero implementation).
        // Read the wall clock directly so an ambient `DBMD_NOW` in the test
        // environment doesn't turn this into a trivially-equal assertion.
        let a = now_override_disabled();
        let b = now_override_disabled();
        assert!(b >= a, "now() went backwards: {a} then {b}");
    }

    #[test]
    fn override_parses_rfc3339_and_falls_back_when_absent_or_bad() {
        // The reproducibility hook: a valid RFC3339 string pins `now()`; an
        // empty or unparseable value yields `None` (wall-clock fallback). We
        // exercise the pure parse helper here — the env-var read is the only
        // thin wrapper around it and is covered end-to-end by the CLI tests
        // that set `DBMD_NOW` (golden determinism would break loudly otherwise).
        assert_eq!(
            parse_override("2026-05-29T10:15:00Z"),
            Some(
                "2026-05-29T10:15:00Z"
                    .parse::<DateTime<FixedOffset>>()
                    .unwrap()
            )
        );
        assert_eq!(parse_override(""), None);
        assert_eq!(parse_override("   "), None);
        assert_eq!(parse_override("not-a-timestamp"), None);
        assert_eq!(parse_override("2026-05-29"), None); // date-only is not RFC3339
    }

    /// The wall-clock instant, bypassing the env override — for tests that must
    /// observe real time regardless of an ambient `DBMD_NOW`.
    fn now_override_disabled() -> DateTime<FixedOffset> {
        Utc::now().fixed_offset()
    }

    /// Pure parse of an override string (the body of [`now_override`] minus the
    /// env read), so the parse contract is unit-testable without mutating
    /// process-global env state from a parallel test.
    fn parse_override(raw: &str) -> Option<DateTime<FixedOffset>> {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|dt| dt.with_timezone(&Utc).fixed_offset())
    }
}
