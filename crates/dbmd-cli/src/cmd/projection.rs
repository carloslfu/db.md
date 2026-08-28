// SPDX-License-Identifier: Apache-2.0

//! Shared loading and stable CLI error mapping for a partial-store policy.

use dbmd_core::projection::ProjectionPolicy;
use dbmd_core::Store;
use std::io::Read;

use crate::error::{CliError, ExitCode};

pub(crate) fn load(store: &Store, path: &str) -> Result<ProjectionPolicy, CliError> {
    ProjectionPolicy::load(store, path)
        .map_err(|error| CliError::new(ExitCode::Runtime, "BAD_PROJECTION_EXCLUDES", error))
}

pub(crate) fn load_manifest(store: &Store, path: &str) -> Result<ProjectionPolicy, CliError> {
    let result = if path == "-" {
        let mut bytes = Vec::new();
        std::io::stdin()
            .take(dbmd_core::projection::MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| {
                CliError::new(
                    ExitCode::Runtime,
                    "BAD_PROJECTION_MANIFEST",
                    format!("could not read projection manifest from stdin: {error}"),
                )
            })?;
        ProjectionPolicy::from_manifest_bytes("stdin", &bytes)
    } else {
        ProjectionPolicy::load_manifest(store, path)
    };
    result.map_err(|error| CliError::new(ExitCode::Runtime, "BAD_PROJECTION_MANIFEST", error))
}
