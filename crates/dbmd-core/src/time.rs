//! Timestamps, and the canonical wall-clock for write-surface seeding.
//!
//! Every write surface that stamps `created` / `updated` (or a log entry's
//! timestamp) seeds it from [`now`] so all of them agree on one representation:
//! the current instant as a UTC-offset [`Timestamp`]. This is the type
//! [`crate::Frontmatter::created`] / [`crate::Frontmatter::updated`] already
//! hold, so callers assign it directly with no string round-trip.
//!
//! Keeping this in `dbmd-core` (rather than re-deriving it per CLI handler)
//! means `dbmd write`, `dbmd fm init`, `dbmd fm set`, and `dbmd log append` all
//! compute "now" the same way from one place — and the thin CLI carries no
//! bespoke calendar logic.
//!
//! ## The `Timestamp` type
//!
//! A db.md timestamp is an instant plus the fixed UTC offset it was written
//! with (`2026-05-27T08:00:00-07:00`). [`Timestamp`] is the toolkit's own small
//! type for that. It replaced `chrono` (unmaintained, RUSTSEC-2026-0543) and
//! keeps the exact rules dbmd had through chrono 0.4, so stores, sidecars and
//! log headers read and render byte-for-byte as before:
//!
//! - [`Timestamp::parse_rfc3339`] — chrono's `DateTime::parse_from_rfc3339`:
//!   `T`, `t` or a space between date and time; `Z`, `z` or `±HH:MM` offsets
//!   (also U+2212 minus) up to ±23:59; any number of fractional-second digits
//!   (the first nine are kept); a `:60` leap second.
//! - [`Timestamp::to_rfc3339`] / [`Timestamp::to_rfc3339_z`] — chrono's
//!   `to_rfc3339` (`+00:00`) and `to_rfc3339_opts(AutoSi, true)` (`Z`):
//!   fractional seconds printed as 3, 6 or 9 digits, only when non-zero.
//! - `FromStr` and serde — chrono's relaxed `FromStr` and serde format, so
//!   `index.jsonl` sidecars written by earlier releases still read.
//! - [`parse_ymd`] / [`Timestamp::parse_minute_utc`] — chrono's
//!   `%Y-%m-%d` and `%Y-%m-%d %H:%M` parsing, including its leniency
//!   (unpadded month and day, leading whitespace before a number).
//!
//! Equality, ordering and hashing compare the instant only: the same moment
//! written at two offsets is equal, exactly as chrono's `DateTime` behaved.
//! The calendar is proleptic Gregorian over chrono's date range (years
//! -262143 to 262142). There is no time-zone database: db.md timestamps only
//! ever carry fixed offsets.
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

use std::fmt::{self, Write as _};
use std::hash::{Hash, Hasher};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

/// Environment variable that pins [`now`] to a fixed RFC3339 instant. See the
/// module docs (`## Reproducibility hook`). Unset ⇒ wall clock.
pub const NOW_OVERRIDE_ENV: &str = "DBMD_NOW";

/// The current instant as a UTC-offset timestamp.
///
/// Returns [`Timestamp`] — the canonical type the universal `created` /
/// `updated` frontmatter fields and the log entry timestamp hold — so write
/// surfaces seed it without any RFC3339 string round-trip. Resolution is
/// nanoseconds; the canonical writers render it to RFC3339 on the way to disk.
///
/// If [`NOW_OVERRIDE_ENV`] (`DBMD_NOW`) is set to a parseable RFC3339 timestamp,
/// that fixed instant is returned instead of the wall clock — the deterministic
/// reproducibility hook (see module docs). An unset or unparseable value falls
/// back to the wall clock, so the default path is unchanged.
pub fn now() -> Timestamp {
    std::env::var(NOW_OVERRIDE_ENV)
        .ok()
        .and_then(|raw| parse_override(&raw))
        .unwrap_or_else(Timestamp::now_utc)
}

/// Parse a [`NOW_OVERRIDE_ENV`] value. `None` when empty or unparseable (the
/// caller then uses the wall clock). Normalized to the UTC (zero) offset to
/// match the wall-clock path, so downstream RFC3339 rendering is offset-stable
/// regardless of the offset the override was written with.
fn parse_override(raw: &str) -> Option<Timestamp> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    Timestamp::parse_rfc3339(raw).ok().map(|ts| ts.to_utc())
}

const NANOS_PER_SEC: u32 = 1_000_000_000;
const SECS_PER_DAY: i64 = 86_400;
/// chrono's `NaiveDate` range, kept so the accepted inputs stay identical.
const MIN_YEAR: i32 = -262_143;
const MAX_YEAR: i32 = 262_142;

/// An instant with the fixed UTC offset it was written with. See the module
/// docs for the parsing and rendering rules.
#[derive(Clone, Copy)]
pub struct Timestamp {
    /// Whole seconds since the Unix epoch (UTC).
    secs: i64,
    /// Nanoseconds past `secs`. `1_000_000_000..2_000_000_000` marks a leap
    /// second (`:60`), stored on second `:59` as chrono did.
    nanos: u32,
    /// Offset east of UTC, in seconds (`|offset| < 86_400`).
    offset: i32,
}

/// Why a timestamp string did not parse. The messages match chrono's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimestampError(ErrorKind);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorKind {
    OutOfRange,
    Invalid,
    TooShort,
    TooLong,
}

impl fmt::Display for TimestampError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0 {
            ErrorKind::OutOfRange => "input is out of range",
            ErrorKind::Invalid => "input contains invalid characters",
            ErrorKind::TooShort => "premature end of input",
            ErrorKind::TooLong => "trailing input",
        })
    }
}

impl std::error::Error for TimestampError {}

type Parse<T> = Result<T, TimestampError>;

fn fail<T>(kind: ErrorKind) -> Parse<T> {
    Err(TimestampError(kind))
}

/// Local calendar fields of a [`Timestamp`] at some offset.
struct Civil {
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    nanos: u32,
}

impl Timestamp {
    /// Build a timestamp from local calendar fields and an offset east of UTC
    /// in seconds. `nanosecond` may reach `1_999_999_999` on second 59 to
    /// express a leap second. `None` for any field out of range.
    #[allow(clippy::too_many_arguments)]
    pub fn from_civil(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
        second: u32,
        nanosecond: u32,
        offset_seconds: i32,
    ) -> Option<Self> {
        if !valid_date(year, month, day)
            || hour > 23
            || minute > 59
            || second > 59
            || nanosecond >= 2 * NANOS_PER_SEC
            || (nanosecond >= NANOS_PER_SEC && second != 59)
            || offset_seconds.unsigned_abs() >= 86_400
        {
            return None;
        }
        let local = days_from_civil(year, month, day) * SECS_PER_DAY
            + i64::from(hour * 3600 + minute * 60 + second);
        let secs = local - i64::from(offset_seconds);
        // chrono also kept the UTC side inside its calendar range.
        let utc_days = secs.div_euclid(SECS_PER_DAY);
        if utc_days < days_from_civil(MIN_YEAR, 1, 1)
            || utc_days > days_from_civil(MAX_YEAR, 12, 31)
        {
            return None;
        }
        Some(Self {
            secs,
            nanos: nanosecond,
            offset: offset_seconds,
        })
    }

    /// The wall clock, at the UTC offset. Ignores `DBMD_NOW`; use [`now`] for
    /// write-surface seeding.
    pub fn now_utc() -> Self {
        let (secs, nanos) = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
            Err(before_epoch) => {
                let d = before_epoch.duration();
                let secs = -(d.as_secs() as i64);
                match d.subsec_nanos() {
                    0 => (secs, 0),
                    n => (secs - 1, NANOS_PER_SEC - n),
                }
            }
        };
        Self {
            secs,
            nanos,
            offset: 0,
        }
    }

    /// Parse an RFC 3339 timestamp with chrono's `parse_from_rfc3339` rules
    /// (see the module docs). The written offset is kept.
    pub fn parse_rfc3339(s: &str) -> Result<Self, TimestampError> {
        let b = s.as_bytes();
        if b.len() < 19 {
            return fail(ErrorKind::TooShort);
        }
        let d = |i: usize| -> Parse<u32> {
            if b[i].is_ascii_digit() {
                Ok(u32::from(b[i] - b'0'))
            } else {
                fail(ErrorKind::Invalid)
            }
        };
        let year = d(0)? * 1000 + d(1)? * 100 + d(2)? * 10 + d(3)?;
        if b[4] != b'-' {
            return fail(ErrorKind::Invalid);
        }
        let month = d(5)? * 10 + d(6)?;
        if b[7] != b'-' {
            return fail(ErrorKind::Invalid);
        }
        let day = d(8)? * 10 + d(9)?;
        // `year` is at most 9999, so the cast is lossless.
        let year = year as i32;
        if !valid_date(year, month, day) {
            return fail(ErrorKind::OutOfRange);
        }
        if !matches!(b[10], b'T' | b't' | b' ') {
            return fail(ErrorKind::Invalid);
        }
        let hour = d(11)? * 10 + d(12)?;
        if b[13] != b':' {
            return fail(ErrorKind::Invalid);
        }
        let minute = d(14)? * 10 + d(15)?;
        if b[16] != b':' {
            return fail(ErrorKind::Invalid);
        }
        let (second, leap) = leap_second(d(17)? * 10 + d(18)?);
        let (rest, nanos) = if b.get(19) == Some(&b'.') {
            let (rest, fraction) = fraction(&s[20..])?;
            (rest, leap + fraction)
        } else {
            (&s[19..], leap)
        };
        if hour > 23 || minute > 59 || second > 59 {
            return fail(ErrorKind::OutOfRange);
        }
        let (rest, offset) = offset(rest, false)?;
        if !rest.is_empty() {
            return fail(ErrorKind::TooLong);
        }
        Self::from_civil(year, month, day, hour, minute, second, nanos, offset)
            .ok_or(TimestampError(ErrorKind::OutOfRange))
    }

    /// Parse a log-header `YYYY-MM-DD HH:MM` (chrono `%Y-%m-%d %H:%M` rules)
    /// as a UTC instant. `None` on any malformed shape.
    pub fn parse_minute_utc(s: &str) -> Option<Self> {
        let (s, (year, month, day)) = ymd_fields(s).ok()?;
        let s = s.trim_start();
        let (s, hour) = field(s, 0, 23).ok()?;
        let s = literal(s, ':').ok()?;
        let (s, minute) = field(s, 0, 59).ok()?;
        if !s.is_empty() {
            return None;
        }
        Self::from_civil(year, month, day, hour, minute, 0, 0, 0)
    }

    /// The same instant at the UTC (zero) offset.
    pub fn to_utc(self) -> Self {
        Self { offset: 0, ..self }
    }

    /// Offset east of UTC, in seconds.
    pub fn offset_seconds(&self) -> i32 {
        self.offset
    }

    /// The calendar year at the timestamp's own offset.
    pub fn year(&self) -> i32 {
        self.civil(self.offset).year
    }

    /// The calendar month (1–12) at the timestamp's own offset.
    pub fn month(&self) -> u32 {
        self.civil(self.offset).month
    }

    /// Nanoseconds since the Unix epoch; `None` outside the `i64` range
    /// (roughly years 1677–2262).
    pub fn unix_nanos(&self) -> Option<i64> {
        let mut secs = self.secs;
        let mut nanos = i64::from(self.nanos);
        // Keep the intermediate product from underflowing near `i64::MIN`.
        if secs < 0 {
            nanos -= i64::from(NANOS_PER_SEC);
            secs += 1;
        }
        secs.checked_mul(i64::from(NANOS_PER_SEC))?
            .checked_add(nanos)
    }

    /// RFC 3339 at the written offset, `+00:00` for UTC
    /// (`2026-05-27T08:00:00-07:00`).
    pub fn to_rfc3339(&self) -> String {
        self.rfc3339(false)
    }

    /// RFC 3339 at the written offset, `Z` for UTC (`2026-05-27T15:00:00Z`).
    /// This is the `index.jsonl` / `index.md` form.
    pub fn to_rfc3339_z(&self) -> String {
        self.rfc3339(true)
    }

    /// The UTC instant with exactly three fractional digits
    /// (`2026-05-27T15:00:00.000Z`), as link.md feed entries carry it.
    pub fn to_rfc3339_utc_millis(&self) -> String {
        let c = self.civil(0);
        let mut out = String::with_capacity(24);
        push_year(&mut out, c.year);
        let _ = write!(
            out,
            "-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            c.month,
            c.day,
            c.hour,
            c.minute,
            c.second + c.nanos / NANOS_PER_SEC,
            c.nanos % NANOS_PER_SEC / 1_000_000
        );
        out
    }

    /// `YYYY-MM-DD` at the written offset.
    pub fn format_date(&self) -> String {
        let c = self.civil(self.offset);
        let mut out = String::with_capacity(10);
        push_year(&mut out, c.year);
        let _ = write!(out, "-{:02}-{:02}", c.month, c.day);
        out
    }

    /// `YYYY-MM-DD HH:MM` at the written offset — the log entry header form.
    pub fn format_minute(&self) -> String {
        let c = self.civil(self.offset);
        let mut out = self.format_date();
        let _ = write!(out, " {:02}:{:02}", c.hour, c.minute);
        out
    }

    fn rfc3339(&self, zulu: bool) -> String {
        let c = self.civil(self.offset);
        let (second, nanos) = if c.nanos >= NANOS_PER_SEC {
            (c.second + 1, c.nanos - NANOS_PER_SEC)
        } else {
            (c.second, c.nanos)
        };
        let mut out = String::with_capacity(32);
        push_year(&mut out, c.year);
        let _ = write!(
            out,
            "-{:02}-{:02}T{:02}:{:02}:{:02}",
            c.month, c.day, c.hour, c.minute, second
        );
        // Fractional seconds only when non-zero, in 3, 6 or 9 digits.
        let _ = match nanos {
            0 => Ok(()),
            n if n % 1_000_000 == 0 => write!(out, ".{:03}", n / 1_000_000),
            n if n % 1_000 == 0 => write!(out, ".{:06}", n / 1_000),
            n => write!(out, ".{n:09}"),
        };
        if zulu && self.offset == 0 {
            out.push('Z');
        } else {
            let (sign, offset) = if self.offset < 0 {
                ('-', -self.offset)
            } else {
                ('+', self.offset)
            };
            let minutes = (offset + 30) / 60;
            let _ = write!(out, "{sign}{:02}:{:02}", minutes / 60, minutes % 60);
        }
        out
    }

    fn civil(&self, offset: i32) -> Civil {
        let local = self.secs + i64::from(offset);
        let (year, month, day) = civil_from_days(local.div_euclid(SECS_PER_DAY));
        // `rem_euclid` keeps the second of the day in `0..86_400`.
        let second_of_day = local.rem_euclid(SECS_PER_DAY) as u32;
        Civil {
            year,
            month,
            day,
            hour: second_of_day / 3600,
            minute: second_of_day / 60 % 60,
            second: second_of_day % 60,
            nanos: self.nanos,
        }
    }
}

impl PartialEq for Timestamp {
    fn eq(&self, other: &Self) -> bool {
        self.secs == other.secs && self.nanos == other.nanos
    }
}

impl Eq for Timestamp {}

impl PartialOrd for Timestamp {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Timestamp {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.secs, self.nanos).cmp(&(other.secs, other.nanos))
    }
}

impl Hash for Timestamp {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.secs.hash(state);
        self.nanos.hash(state);
    }
}

impl fmt::Debug for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_rfc3339())
    }
}

impl fmt::Display for Timestamp {
    /// RFC 3339 with `+00:00` for UTC, the same as [`Timestamp::to_rfc3339`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_rfc3339())
    }
}

impl FromStr for Timestamp {
    type Err = TimestampError;

    /// chrono's relaxed RFC 3339 reading: whitespace is allowed around the
    /// fields, years may be signed and longer than four digits, the offset
    /// may be `UTC` or omit its colon, and trailing whitespace is ignored.
    fn from_str(s: &str) -> Result<Self, TimestampError> {
        let (s, year) = year_field(s)?;
        let s = literal(s.trim_start(), '-')?;
        let (s, month) = field(s, 1, 12)?;
        let s = literal(s.trim_start(), '-')?;
        let (s, day) = field(s, 1, 31)?;
        let s = match s.as_bytes().first() {
            Some(b'T' | b't' | b' ') => &s[1..],
            Some(_) => return fail(ErrorKind::Invalid),
            None => return fail(ErrorKind::TooShort),
        };
        let (s, hour) = field(s, 0, 23)?;
        let s = literal(s.trim_start(), ':')?;
        let (s, minute) = field(s, 0, 59)?;
        let s = literal(s.trim_start(), ':')?;
        let (s, second) = field(s, 0, 60)?;
        let (s, fraction) = match s.strip_prefix('.') {
            Some(rest) => fraction(rest)?,
            None => (s, 0),
        };
        let s = s.trim_start();
        let (s, offset) = if s.len() >= 3 && s.as_bytes()[..3].eq_ignore_ascii_case(b"UTC") {
            (&s[3..], 0)
        } else {
            offset(s, true)?
        };
        if !s.trim_start().is_empty() {
            return fail(ErrorKind::TooLong);
        }
        let (second, leap) = leap_second(second);
        Self::from_civil(
            year,
            month,
            day,
            hour,
            minute,
            second,
            leap + fraction,
            offset,
        )
        .ok_or(TimestampError(ErrorKind::OutOfRange))
    }
}

impl serde::Serialize for Timestamp {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_rfc3339_z())
    }
}

impl<'de> serde::Deserialize<'de> for Timestamp {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = Timestamp;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an RFC 3339 formatted date and time string")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Timestamp, E> {
                value.parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_str(Visitor)
    }
}

/// Parse a calendar date with chrono's `%Y-%m-%d` rules: an optionally signed
/// year, an unpadded month or day, and whitespace before each number are
/// accepted; trailing input is not. Returns `(year, month, day)`.
pub fn parse_ymd(s: &str) -> Option<(i32, u32, u32)> {
    match ymd_fields(s) {
        Ok(("", date)) => Some(date),
        _ => None,
    }
}

/// `%Y-%m-%d` fields followed by whatever input remains, date validated.
fn ymd_fields(s: &str) -> Parse<(&str, (i32, u32, u32))> {
    let (s, year) = year_field(s)?;
    let s = literal(s, '-')?;
    let (s, month) = field(s, 1, 12)?;
    let s = literal(s, '-')?;
    let (s, day) = field(s, 1, 31)?;
    if !valid_date(year, month, day) {
        return fail(ErrorKind::OutOfRange);
    }
    Ok((s, (year, month, day)))
}

/// Second 60 is a leap second: second 59 plus a full second of nanoseconds.
fn leap_second(second: u32) -> (u32, u32) {
    if second == 60 {
        (59, NANOS_PER_SEC)
    } else {
        (second, 0)
    }
}

/// chrono's `scan::number`: `min..=max` ASCII digits.
fn number(s: &str, min: usize, max: usize) -> Parse<(&str, i64)> {
    let bytes = s.as_bytes();
    if bytes.len() < min {
        return fail(ErrorKind::TooShort);
    }
    let mut n: i64 = 0;
    for (i, &c) in bytes.iter().take(max).enumerate() {
        if !c.is_ascii_digit() {
            return if i < min {
                fail(ErrorKind::Invalid)
            } else {
                Ok((&s[i..], n))
            };
        }
        n = n
            .checked_mul(10)
            .and_then(|n| n.checked_add(i64::from(c - b'0')))
            .ok_or(TimestampError(ErrorKind::OutOfRange))?;
    }
    Ok((&s[max.min(bytes.len())..], n))
}

/// A one- or two-digit field after optional whitespace, range-checked.
fn field(s: &str, min: u32, max: u32) -> Parse<(&str, u32)> {
    let (s, v) = number(s.trim_start(), 1, 2)?;
    // At most two digits, so the cast is lossless.
    let v = v as u32;
    if !(min..=max).contains(&v) {
        return fail(ErrorKind::OutOfRange);
    }
    Ok((s, v))
}

/// chrono's `%Y`: up to four digits, or a signed year of any length.
fn year_field(s: &str) -> Parse<(&str, i32)> {
    let s = s.trim_start();
    let (s, year) = if let Some(rest) = s.strip_prefix('-') {
        let (s, v) = number(rest, 1, usize::MAX)?;
        (s, -v)
    } else if let Some(rest) = s.strip_prefix('+') {
        number(rest, 1, usize::MAX)?
    } else {
        number(s, 1, 4)?
    };
    let year = i32::try_from(year).map_err(|_| TimestampError(ErrorKind::OutOfRange))?;
    Ok((s, year))
}

fn literal(s: &str, c: char) -> Parse<&str> {
    match s.strip_prefix(c) {
        Some(rest) => Ok(rest),
        None if s.is_empty() => fail(ErrorKind::TooShort),
        None => fail(ErrorKind::Invalid),
    }
}

/// chrono's `scan::nanosecond`: one to nine digits scaled to nanoseconds;
/// further digits are skipped.
fn fraction(s: &str) -> Parse<(&str, u32)> {
    const SCALE: [i64; 10] = [
        0,
        100_000_000,
        10_000_000,
        1_000_000,
        100_000,
        10_000,
        1_000,
        100,
        10,
        1,
    ];
    let (rest, v) = number(s, 1, 9)?;
    let digits = s.len() - rest.len();
    // At most nine digits scaled to nanoseconds: below 1_000_000_000.
    let nanos = (v * SCALE[digits]) as u32;
    Ok((rest.trim_start_matches(|c: char| c.is_ascii_digit()), nanos))
}

/// chrono's `scan::timezone_offset`: `Z`/`z`, or a sign (`+`, `-` or U+2212)
/// with two-digit hours and minutes. Strict mode needs a `:` between them;
/// relaxed mode skips any run of `:` and whitespace. Returns seconds east.
fn offset(s: &str, relaxed: bool) -> Parse<(&str, i32)> {
    let mut chars = s.chars();
    let negative = match chars.next() {
        Some('Z' | 'z') => return Ok((chars.as_str(), 0)),
        Some('+') => false,
        Some('-' | '\u{2212}') => true,
        Some(_) => return fail(ErrorKind::Invalid),
        None => return fail(ErrorKind::TooShort),
    };
    let s = chars.as_str();
    let (s, hours) = two_digits(s)?;
    let s = if relaxed {
        s.trim_start_matches(|c: char| c == ':' || c.is_whitespace())
    } else {
        literal(s, ':')?
    };
    let b = s.as_bytes();
    if b.len() < 2 {
        return fail(ErrorKind::TooShort);
    }
    let minutes = match (b[0], b[1]) {
        (m1 @ b'0'..=b'5', m2 @ b'0'..=b'9') => i32::from((m1 - b'0') * 10 + (m2 - b'0')),
        (b'6'..=b'9', b'0'..=b'9') => return fail(ErrorKind::OutOfRange),
        _ => return fail(ErrorKind::Invalid),
    };
    let seconds = hours * 3600 + minutes * 60;
    Ok((&s[2..], if negative { -seconds } else { seconds }))
}

fn two_digits(s: &str) -> Parse<(&str, i32)> {
    let b = s.as_bytes();
    if b.len() < 2 {
        return fail(ErrorKind::TooShort);
    }
    if !(b[0].is_ascii_digit() && b[1].is_ascii_digit()) {
        return fail(ErrorKind::Invalid);
    }
    Ok((&s[2..], i32::from((b[0] - b'0') * 10 + (b[1] - b'0'))))
}

fn push_year(out: &mut String, year: i32) {
    // ISO 8601 signs years outside 0000–9999, as chrono did.
    let _ = if (0..=9999).contains(&year) {
        write!(out, "{year:04}")
    } else {
        write!(out, "{year:+05}")
    };
}

fn is_leap_year(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn valid_date(year: i32, month: u32, day: u32) -> bool {
    (MIN_YEAR..=MAX_YEAR).contains(&year)
        && (1..=12).contains(&month)
        && day >= 1
        && day <= days_in_month(year, month)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let y = i64::from(year) - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let year_of_era = y.rem_euclid(400);
    let m = i64::from(month);
    let day_of_year = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The proleptic Gregorian date `days` after 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year as i32, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        Timestamp::parse_rfc3339(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn now_is_utc_offset() {
        // The canonical seed is always emitted at the UTC (zero) offset, so two
        // calls and any downstream RFC3339 rendering stay timezone-stable. Hold
        // for both paths: explicitly pin a non-UTC-offset override and confirm
        // it is normalized to the zero offset (so a `DBMD_NOW` set in the
        // ambient env can never make the seed timezone-unstable).
        assert_eq!(Timestamp::now_utc().offset_seconds(), 0);
        let pinned = parse_override("2026-05-29T12:00:00+05:30").expect("valid override");
        assert_eq!(pinned.offset_seconds(), 0);
        assert_eq!(pinned, "2026-05-29T06:30:00Z".parse::<Timestamp>().unwrap());
        assert_eq!(pinned.to_rfc3339(), "2026-05-29T06:30:00+00:00");
    }

    #[test]
    fn now_is_monotonic_nondecreasing() {
        // Wall-clock now must not go backwards between two reads in the same
        // process (guards against an accidental fixed/zero implementation).
        // Read the wall clock directly so an ambient `DBMD_NOW` in the test
        // environment doesn't turn this into a trivially-equal assertion.
        let a = Timestamp::now_utc();
        let b = Timestamp::now_utc();
        assert!(b >= a, "now() went backwards: {a} then {b}");
        assert!(a.year() >= 2026);
    }

    #[test]
    fn override_parses_rfc3339_and_falls_back_when_absent_or_bad() {
        // The reproducibility hook: a valid RFC3339 string pins `now()`; an
        // empty or unparseable value yields `None` (wall-clock fallback). The
        // env-var read is the only thin wrapper around this and is covered
        // end-to-end by the CLI tests that set `DBMD_NOW` (golden determinism
        // would break loudly otherwise).
        assert_eq!(
            parse_override("2026-05-29T10:15:00Z"),
            Some(ts("2026-05-29T10:15:00Z"))
        );
        assert_eq!(parse_override(""), None);
        assert_eq!(parse_override("   "), None);
        assert_eq!(parse_override("not-a-timestamp"), None);
        assert_eq!(parse_override("2026-05-29"), None); // date-only is not RFC3339
    }

    #[test]
    fn rfc3339_round_trips_the_written_offset() {
        for s in [
            "2026-05-27T08:00:00-07:00",
            "2026-05-27T08:00:00+05:30",
            "2026-05-27T08:00:00.500+00:00",
            "2026-05-27T08:00:00.123456+00:00",
            "2026-05-27T08:00:00.123456789+00:00",
            "0000-01-01T00:00:00+00:00",
            "9999-12-31T23:59:59+00:00",
        ] {
            assert_eq!(ts(s).to_rfc3339(), s);
        }
        let utc = ts("2026-05-29t12:00:00.1z");
        assert_eq!(utc.to_rfc3339(), "2026-05-29T12:00:00.100+00:00");
        assert_eq!(utc.to_rfc3339_z(), "2026-05-29T12:00:00.100Z");
        assert_eq!(utc.to_rfc3339_utc_millis(), "2026-05-29T12:00:00.100Z");
        // Digits past the ninth are dropped; U+2212 is a minus sign.
        assert_eq!(
            ts("2026-05-29 12:00:00.1234567891\u{2212}01:00").to_rfc3339_z(),
            "2026-05-29T12:00:00.123456789-01:00"
        );
        let shifted = ts("2026-01-01T00:30:00+01:00");
        assert_eq!((shifted.year(), shifted.month()), (2026, 1));
        let in_utc = shifted.to_utc();
        assert_eq!((in_utc.year(), in_utc.month()), (2025, 12));
        assert_eq!(in_utc.to_rfc3339_z(), "2025-12-31T23:30:00Z");
        assert_eq!(in_utc.format_minute(), "2025-12-31 23:30");
        // One instant at two offsets is one timestamp.
        assert_eq!(shifted, in_utc);
        assert_eq!(
            ts("1970-01-01T00:00:01.5Z").unix_nanos(),
            Some(1_500_000_000)
        );
        assert_eq!(
            ts("1969-12-31T23:59:59.5Z").unix_nanos(),
            Some(-500_000_000)
        );
        // Out-of-range years get an ISO 8601 sign.
        assert_eq!(
            ts("0000-01-01T00:00:00+01:00").to_utc().to_rfc3339_z(),
            "-0001-12-31T23:00:00Z"
        );
    }

    #[test]
    fn rfc3339_rejects_what_chrono_rejected() {
        for s in [
            "2026-05-29",
            "2026-05-29T12:00Z",
            "2026-05-29T12:00:00",
            "2026-02-29T12:00:00Z",
            "2026-05-29T24:00:00Z",
            "2026-05-29T12:00:00+24:00",
            "2026-05-29T12:00:00+05:60",
            "2026-05-29T12:00:00+0530",
            "2026-05-29T12:00:00.Z",
            "2026-05-29T12:00:00Z ",
            " 2026-05-29T12:00:00Z",
            "2026-5-29T12:00:00Z",
            "2026-05-29X12:00:00Z",
        ] {
            assert!(Timestamp::parse_rfc3339(s).is_err(), "{s} should not parse");
        }
    }

    #[test]
    fn leap_seconds_render_as_60_and_sort_in_place() {
        let leap = ts("2016-12-31T23:59:60.5Z");
        assert_eq!(leap.to_rfc3339_z(), "2016-12-31T23:59:60.500Z");
        assert_eq!(leap.to_rfc3339_utc_millis(), "2016-12-31T23:59:60.500Z");
        assert!(ts("2016-12-31T23:59:59.999Z") < leap);
        assert!(leap < ts("2017-01-01T00:00:00Z"));
    }

    #[test]
    fn relaxed_from_str_reads_chrono_serde_output() {
        let expected = ts("2026-05-29T12:00:00+05:30");
        for s in [
            "2026-05-29T12:00:00+05:30",
            " 2026-5-29T12:00:00 +05:30 ",
            "2026-05-29 12:00:00+0530",
            "+2026-05-29T12:00:00+05:30",
        ] {
            assert_eq!(s.parse::<Timestamp>(), Ok(expected), "{s}");
        }
        assert_eq!("2026-05-29 06:30:00 UTC".parse::<Timestamp>(), Ok(expected));
        let json = serde_json::to_string(&expected).unwrap();
        assert_eq!(json, "\"2026-05-29T12:00:00+05:30\"");
        assert_eq!(serde_json::from_str::<Timestamp>(&json).unwrap(), expected);
        assert!(serde_json::from_str::<Timestamp>("\"2026-05-29\"").is_err());
    }

    #[test]
    fn date_and_minute_parsers_keep_chrono_leniency() {
        assert_eq!(parse_ymd("2026-05-07"), Some((2026, 5, 7)));
        assert_eq!(parse_ymd("2026-5-7"), Some((2026, 5, 7)));
        assert_eq!(parse_ymd("2024-02-29"), Some((2024, 2, 29)));
        assert_eq!(parse_ymd("2026-02-29"), None);
        assert_eq!(parse_ymd("2026-05-07 "), None);
        assert_eq!(parse_ymd("2026/05/07"), None);
        assert_eq!(parse_ymd("2026-13-01"), None);
        let minute = Timestamp::parse_minute_utc("2026-05-07 09:05").unwrap();
        assert_eq!(minute.to_rfc3339_z(), "2026-05-07T09:05:00Z");
        assert_eq!(minute.format_minute(), "2026-05-07 09:05");
        assert_eq!(minute.format_date(), "2026-05-07");
        assert!(Timestamp::parse_minute_utc("2026-05-07 9:5").is_some());
        assert!(Timestamp::parse_minute_utc("2026-05-07 24:00").is_none());
        assert!(Timestamp::parse_minute_utc("2026-05-07T09:05").is_none());
        assert!(Timestamp::parse_minute_utc("2026-05-07 09:05:00").is_none());
    }

    #[test]
    fn calendar_matches_day_by_day_counting() {
        // Walk the days from 1600 to 2400 one at a time with the month table
        // and check both conversions agree at every step.
        let (mut y, mut m, mut d) = (1600, 1, 1);
        let mut days = days_from_civil(1600, 1, 1);
        while y < 2400 {
            assert_eq!(days_from_civil(y, m, d), days, "{y}-{m}-{d}");
            assert_eq!(civil_from_days(days), (y, m, d), "day {days}");
            days += 1;
            d += 1;
            if d > days_in_month(y, m) {
                d = 1;
                m += 1;
                if m > 12 {
                    m = 1;
                    y += 1;
                }
            }
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(
            civil_from_days(days_from_civil(MIN_YEAR, 1, 1)),
            (MIN_YEAR, 1, 1)
        );
        assert_eq!(
            civil_from_days(days_from_civil(MAX_YEAR, 12, 31)),
            (MAX_YEAR, 12, 31)
        );
    }
}
