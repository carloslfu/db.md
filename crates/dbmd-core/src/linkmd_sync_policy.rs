// SPDX-License-Identifier: Apache-2.0

use std::path::Path;

use sha2::{Digest as _, Sha256};

use crate::projection::ProjectionPolicy;
use crate::store::Store;

const FILE: &str = ".sevralocal";
const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct SyncPolicy {
    projection: ProjectionPolicy,
    pub(crate) digest: String,
}

impl SyncPolicy {
    pub(crate) fn keeps_home(&self, path: &str) -> bool {
        self.projection.excludes_path(path)
    }
}

pub(crate) fn load(store: &Store) -> Result<SyncPolicy, String> {
    let exists = store
        .regular_file_exists(Path::new(FILE))
        .map_err(|_| format!("refusing {FILE}: it is not a no-follow regular file"))?;
    let bytes = if exists {
        store
            .read_bounded(Path::new(FILE), MAX_BYTES + 1)
            .map_err(|_| format!("could not securely read {FILE}"))?
    } else {
        Vec::new()
    };
    if bytes.len() as u64 > MAX_BYTES {
        return Err(format!("refusing {FILE}: it exceeds {MAX_BYTES} bytes"));
    }
    let projection = if exists {
        ProjectionPolicy::load(store, FILE)?
    } else {
        ProjectionPolicy::empty()
    };
    let digest = if exists {
        format!("{:x}", Sha256::digest(&bytes))
    } else {
        format!("{:x}", Sha256::digest(b"link.md-v2:sevralocal:absent"))
    };
    Ok(SyncPolicy { projection, digest })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(policy: Option<&str>) -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("DB.md"),
            "---\nname: Policy test\n---\n",
        )
        .unwrap();
        if let Some(policy) = policy {
            std::fs::write(directory.path().join(FILE), policy).unwrap();
        }
        let store = Store::open_strict(directory.path()).unwrap();
        (directory, store)
    }

    #[test]
    fn policy_is_case_sensitive_and_never_rides() {
        let (_directory, store) = store_with(Some("records/private/**\n"));
        let policy = load(&store).unwrap();
        assert!(policy.keeps_home("records/private/a.md"));
        assert!(!policy.keeps_home("records/Private/a.md"));
        assert!(!policy.keeps_home(FILE));
    }

    #[test]
    fn policy_cannot_hide_the_contract() {
        let (_directory, store) = store_with(Some("**\n"));
        assert!(load(&store).unwrap_err().contains("cannot cover"));
    }
}
