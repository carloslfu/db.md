// SPDX-License-Identifier: Apache-2.0

//! Explicit partial-store projection policies.

use std::collections::BTreeSet;
use std::path::Path;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::Store;

const MAX_BYTES: u64 = 1024 * 1024;
const MAX_LINE_BYTES: usize = 4096;
const MAX_ENTRIES: usize = 10_000;
/// Maximum encoded size of a projection commitment manifest.
pub const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;
const MAX_MANIFEST_HASHES: usize = 100_000;
const PATH_HASH_DOMAIN: &[u8] = b"dbmd-projection-path-v1\0";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectionManifest {
    version: u8,
    algorithm: String,
    path_hashes: Vec<String>,
}

/// A bounded, case-sensitive set of store-relative path rules. Its syntax is
/// intentionally identical to `.sevralocal`: blank lines and `#` comments are
/// ignored; every other line is one glob with backslash escaping enabled.
#[derive(Clone, Debug)]
pub struct ProjectionPolicy {
    set: GlobSet,
    path_hashes: BTreeSet<String>,
}

impl ProjectionPolicy {
    /// Load a required policy file through the store's no-follow capability.
    pub fn load(store: &Store, file: &str) -> Result<Self, String> {
        let bytes = read_regular_bounded(store, file, MAX_BYTES)?;
        let raw =
            std::str::from_utf8(&bytes).map_err(|_| format!("refusing {file}: it is not UTF-8"))?;
        Self::compile(file, raw)
    }

    /// Load a path-confidential projection commitment manifest. The manifest
    /// carries only domain-separated SHA-256 commitments to exact store paths,
    /// so a recovery package need not publish the source `.sevralocal` rules.
    pub fn load_manifest(store: &Store, file: &str) -> Result<Self, String> {
        let bytes = read_regular_bounded(store, file, MAX_MANIFEST_BYTES)?;
        Self::from_manifest_bytes(file, &bytes)
    }

    /// Parse bounded manifest bytes supplied by a trusted higher-level
    /// transport (for example stdin from a signed package verifier).
    pub fn from_manifest_bytes(file: &str, bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(format!(
                "refusing {file}: it exceeds {MAX_MANIFEST_BYTES} bytes"
            ));
        }
        let manifest: ProjectionManifest = serde_json::from_slice(bytes)
            .map_err(|_| format!("refusing {file}: it is not a valid projection manifest"))?;
        if manifest.version != 1 || manifest.algorithm != "sha256" {
            return Err(format!(
                "refusing {file}: unsupported projection manifest format"
            ));
        }
        if manifest.path_hashes.len() > MAX_MANIFEST_HASHES {
            return Err(format!(
                "refusing {file}: it has more than {MAX_MANIFEST_HASHES} path commitments"
            ));
        }
        let mut prior: Option<&str> = None;
        for hash in &manifest.path_hashes {
            if hash.len() != 64
                || !hash
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            {
                return Err(format!(
                    "refusing {file}: projection commitments must be lowercase SHA-256"
                ));
            }
            if prior.is_some_and(|value| value >= hash.as_str()) {
                return Err(format!(
                    "refusing {file}: projection commitments must be sorted and unique"
                ));
            }
            prior = Some(hash);
        }
        let path_hashes = manifest.path_hashes.into_iter().collect::<BTreeSet<_>>();
        if ["DB.md", "assets.jsonl"]
            .into_iter()
            .any(|path| path_hashes.contains(&projection_path_sha256(path)))
        {
            return Err(format!(
                "refusing {file}: projection manifest cannot cover DB.md or assets.jsonl"
            ));
        }
        Ok(Self {
            set: empty_glob_set(),
            path_hashes,
        })
    }

    pub(crate) fn empty() -> Self {
        Self {
            set: empty_glob_set(),
            path_hashes: BTreeSet::new(),
        }
    }

    fn compile(file: &str, raw: &str) -> Result<Self, String> {
        let mut builder = GlobSetBuilder::new();
        let mut effective = 0_usize;
        for (index, line) in raw.lines().enumerate() {
            if line.len() > MAX_LINE_BYTES {
                return Err(format!(
                    "refusing {file}: line {} exceeds {MAX_LINE_BYTES} bytes",
                    index + 1
                ));
            }
            let entry = line.strip_suffix('\r').unwrap_or(line);
            if entry.trim().is_empty() || entry.starts_with('#') {
                continue;
            }
            if entry.starts_with('/')
                || entry.contains('\0')
                || entry
                    .split('/')
                    .any(|component| component.is_empty() || matches!(component, "." | ".."))
            {
                return Err(format!(
                    "refusing {file}: line {} is not a safe store-relative glob",
                    index + 1
                ));
            }
            effective += 1;
            if effective > MAX_ENTRIES {
                return Err(format!(
                    "refusing {file}: it has more than {MAX_ENTRIES} entries"
                ));
            }
            builder.add(
                GlobBuilder::new(entry)
                    .backslash_escape(true)
                    .build()
                    .map_err(|_| {
                        format!("refusing {file}: line {} is not a valid glob", index + 1)
                    })?,
            );
        }
        let set = builder
            .build()
            .map_err(|_| format!("refusing {file}: its matcher could not be compiled"))?;
        if set.is_match("DB.md") || set.is_match("assets.jsonl") {
            return Err(format!(
                "refusing {file}: projection policy cannot cover DB.md or assets.jsonl"
            ));
        }
        Ok(Self {
            set,
            path_hashes: BTreeSet::new(),
        })
    }

    /// True when this exact materialized store path is declared outside the
    /// projection.
    pub fn excludes_path(&self, path: &str) -> bool {
        self.set.is_match(path) || self.path_hashes.contains(&projection_path_sha256(path))
    }

    /// Match a structured wiki target, accounting for markdown's conventional
    /// extensionless link coordinate while retaining raw-source paths.
    pub fn excludes_wiki_coordinate(&self, coordinate: &str) -> bool {
        self.excludes_path(coordinate) || self.excludes_path(&format!("{coordinate}.md"))
    }
}

fn empty_glob_set() -> GlobSet {
    GlobSetBuilder::new()
        .build()
        .expect("an empty projection matcher compiles")
}

fn read_regular_bounded(store: &Store, file: &str, max: u64) -> Result<Vec<u8>, String> {
    let exists = store
        .regular_file_exists(Path::new(file))
        .map_err(|_| format!("refusing {file}: it is not a no-follow regular file"))?;
    if !exists {
        return Err(format!(
            "refusing {file}: it is not a no-follow regular file"
        ));
    }
    let bytes = store
        .read_bounded(Path::new(file), max + 1)
        .map_err(|_| format!("could not securely read {file}"))?;
    if bytes.len() as u64 > max {
        return Err(format!("refusing {file}: it exceeds {max} bytes"));
    }
    Ok(bytes)
}

/// Domain-separated commitment used by projection manifests.
pub fn projection_path_sha256(path: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(PATH_HASH_DOMAIN);
    digest.update(path.as_bytes());
    format!("{:x}", digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(policy: &str) -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("DB.md"),
            "---\ntype: db-md\nscope: test\nowner: test\n---\n",
        )
        .unwrap();
        std::fs::write(directory.path().join(".projection"), policy).unwrap();
        let store = Store::open_strict(directory.path()).unwrap();
        (directory, store)
    }

    #[test]
    fn exact_glob_comment_and_markdown_coordinates_share_one_contract() {
        let (_directory, store) = store_with("# private\nrecords/private/**\nsources/raw/a.json\n");
        let policy = ProjectionPolicy::load(&store, ".projection").unwrap();
        assert!(policy.excludes_path("records/private/a.md"));
        assert!(policy.excludes_wiki_coordinate("records/private/a"));
        assert!(policy.excludes_wiki_coordinate("sources/raw/a.json"));
        assert!(!policy.excludes_path("records/Private/a.md"));
    }

    #[test]
    fn projection_cannot_hide_core_coordinates() {
        let (_directory, store) = store_with("**\n");
        assert!(ProjectionPolicy::load(&store, ".projection")
            .unwrap_err()
            .contains("cannot cover"));
    }

    #[test]
    fn commitment_manifest_matches_exact_paths_without_publishing_them() {
        let private = projection_path_sha256("records/private/secret.md");
        let raw =
            format!("{{\"version\":1,\"algorithm\":\"sha256\",\"path_hashes\":[\"{private}\"]}}");
        let policy = ProjectionPolicy::from_manifest_bytes("stdin", raw.as_bytes()).unwrap();
        assert!(policy.excludes_wiki_coordinate("records/private/secret"));
        assert!(!policy.excludes_path("records/private/other.md"));
        assert!(!raw.contains("secret.md"));
    }

    #[test]
    fn commitment_manifest_is_canonical_and_cannot_hide_core_files() {
        let mut commitments = [
            projection_path_sha256("records/private/first.md"),
            projection_path_sha256("records/private/second.md"),
        ];
        commitments.sort();
        let [first, second] = commitments;
        let unsorted = format!(
            "{{\"version\":1,\"algorithm\":\"sha256\",\"path_hashes\":[\"{second}\",\"{first}\"]}}"
        );
        assert!(
            ProjectionPolicy::from_manifest_bytes("stdin", unsorted.as_bytes())
                .unwrap_err()
                .contains("sorted and unique")
        );
        let core = projection_path_sha256("DB.md");
        let raw =
            format!("{{\"version\":1,\"algorithm\":\"sha256\",\"path_hashes\":[\"{core}\"]}}");
        assert!(
            ProjectionPolicy::from_manifest_bytes("stdin", raw.as_bytes())
                .unwrap_err()
                .contains("cannot cover")
        );
    }
}
