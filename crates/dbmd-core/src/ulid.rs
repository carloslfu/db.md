//! Lowercase ULID minting and validation (SPEC § The `id` field, format v0.4).
//!
//! A [ULID](https://github.com/ulid/spec) is a 128-bit identifier — a 48-bit
//! millisecond timestamp followed by 80 random bits — rendered as 26
//! characters of Crockford base32. db.md's canonical form is **lowercase**
//! (YAML-clean, shell-friendly, reads like the rest of the frontmatter).
//! `dbmd write` mints one for every new content file that carries no `id`;
//! the leading timestamp makes freshly minted ids time-sortable, and the 80
//! random bits make offline minting coordination-free.
//!
//! **Std-only by design.** The toolkit's dependency discipline (zero AI deps,
//! minimal tree) rules out pulling a `ulid`/`rand` stack for one mint call.
//! Randomness comes from [`RandomState`] — std's hasher keys are seeded from
//! OS entropy — mixed with the wall clock, the PID, and a process-global
//! counter. That is not cryptographic randomness and does not need to be:
//! the id's contract is store-scoped uniqueness (`DUP_ID` is the backstop),
//! not unguessability.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Crockford base32, lowercase: `0-9` then `a-z` minus `i`, `l`, `o`, `u`.
const CROCKFORD_LOWER: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// Per-process mint counter: guarantees two mints in the same nanosecond
/// still hash distinct inputs.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Two OS-entropy-seeded hash keys, fixed for the process. Hashing the
/// (time, pid, counter) tuple under two independent keys yields 128 bits of
/// well-distributed output per mint; the mint takes 80 of them.
fn hash_states() -> &'static (RandomState, RandomState) {
    static STATES: OnceLock<(RandomState, RandomState)> = OnceLock::new();
    STATES.get_or_init(|| (RandomState::new(), RandomState::new()))
}

/// 80 bits of per-mint randomness (in the low bits of the returned value).
fn entropy80() -> u128 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();

    let (a, b) = hash_states();
    let hi = a.hash_one((nanos, count, pid));
    let lo = b.hash_one((count, pid, nanos));

    (((hi as u128) << 64) | (lo as u128)) & ((1u128 << 80) - 1)
}

/// Mint a fresh lowercase ULID: 48-bit Unix-millisecond timestamp + 80
/// random bits, encoded as 26 chars of lowercase Crockford base32.
pub fn mint() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
        & 0xFFFF_FFFF_FFFF; // 48 bits
    encode(((ms as u128) << 80) | entropy80())
}

/// Encode a 128-bit value as 26 lowercase Crockford base32 chars. The first
/// char carries only the top 3 bits (the 128-bit value viewed as 130 bits
/// with two leading zeros), so it is always `0`–`7`.
fn encode(value: u128) -> String {
    let mut out = String::with_capacity(26);
    for i in 0..26 {
        let shift = 125 - 5 * i;
        out.push(CROCKFORD_LOWER[((value >> shift) & 0x1F) as usize] as char);
    }
    out
}

/// True when `s` is a well-formed **lowercase** ULID: exactly 26 chars, all
/// lowercase Crockford base32, first char `0`–`7` (so the value fits 128
/// bits). Uppercase or mixed-case forms are not the db.md canonical form and
/// return false — as does any other opaque id, which stays *legal* in a
/// store (SPEC: the ULID form is recommended, never a validation gate).
pub fn is_ulid(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() == 26
        && matches!(bytes[0], b'0'..=b'7')
        && bytes.iter().all(|b| CROCKFORD_LOWER.contains(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn mint_is_wellformed_lowercase_ulid() {
        for _ in 0..100 {
            let id = mint();
            assert!(is_ulid(&id), "minted id {id:?} is not a lowercase ULID");
            assert_eq!(id.len(), 26);
            assert_eq!(id, id.to_lowercase());
        }
    }

    #[test]
    fn mint_is_unique_across_a_burst() {
        // 10k mints in a tight loop (same millisecond for most): all distinct.
        let ids: HashSet<String> = (0..10_000).map(|_| mint()).collect();
        assert_eq!(ids.len(), 10_000, "duplicate ULIDs in a same-ms burst");
    }

    #[test]
    fn mint_is_time_sortable_across_ms_boundaries() {
        // Two mints separated by >1ms sort in mint order (the 48-bit ms
        // timestamp is the most significant component).
        let a = mint();
        std::thread::sleep(std::time::Duration::from_millis(3));
        let b = mint();
        assert!(a < b, "ULIDs did not time-sort: {a} !< {b}");
    }

    #[test]
    fn timestamp_prefix_decodes_to_now() {
        // Decode the 10-char timestamp prefix back to ms and compare to the
        // wall clock — proves the encoding puts the timestamp in the right
        // bits (not just that output "looks like" a ULID).
        let before_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let id = mint();
        let after_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let mut ts: u64 = 0;
        for b in id.as_bytes().iter().take(10) {
            let digit = CROCKFORD_LOWER.iter().position(|c| c == b).unwrap() as u64;
            ts = (ts << 5) | digit;
        }
        assert!(
            (before_ms..=after_ms).contains(&ts),
            "decoded ts {ts} outside [{before_ms}, {after_ms}]"
        );
    }

    #[test]
    fn is_ulid_accepts_only_canonical_lowercase() {
        assert!(is_ulid("01j5qc3v9k4ym8rwbn2tqe6f7d"));
        assert!(is_ulid("00000000000000000000000000"));
        assert!(is_ulid("7zzzzzzzzzzzzzzzzzzzzzzzzz")); // max 128-bit value
                                                        // Wrong length.
        assert!(!is_ulid(""));
        assert!(!is_ulid("01j5qc3v9k4ym8rwbn2tqe6f7"));
        assert!(!is_ulid("01j5qc3v9k4ym8rwbn2tqe6f7dd"));
        // Uppercase (the upstream ULID spelling) is not db.md-canonical.
        assert!(!is_ulid("01J5QC3V9K4YM8RWBN2TQE6F7D"));
        // Excluded Crockford letters and non-base32 bytes.
        assert!(!is_ulid("01j5qc3v9k4ym8rwbn2tqe6fil"));
        assert!(!is_ulid("01j5qc3v9k4ym8rwbn2tqe6f7-"));
        // First char beyond `7` overflows 128 bits.
        assert!(!is_ulid("8zzzzzzzzzzzzzzzzzzzzzzzzz"));
        // A plain slug is not a ULID (it stays LEGAL as an id; just not this form).
        assert!(!is_ulid("sarah-chen"));
    }
}
