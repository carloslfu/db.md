//! `dbmd assets <sub>` — the heavy-binary asset manifest.
//!
//! Arg-parse + format glue only; all logic lives in [`dbmd_core::assets`].
//! Dispatches the six leaves:
//!   - `scan`   → discover declarations, hash present files, rewrite `assets.jsonl`
//!   - `refresh` → re-hash one declared asset and update its manifest row
//!   - `refresh-wrapper` → reconcile one wrapper's complete asset set in one write
//!   - `verify` → byte-completeness gate (exits non-zero when incomplete)
//!   - `status` → present / missing report (never fails)
//!   - `paths`  → the store-relative path list (for an ignore mechanism)
//!
//! None of these runs git or touches the network. Keeping bytes out of a VCS is
//! the harness's job; `dbmd assets paths` is the VCS-neutral list it consumes.

use std::path::Path;

use crate::cli::{
    AssetsArgs, AssetsCommand, AssetsPathsArgs, AssetsRefreshArgs, AssetsRefreshWrapperArgs,
    AssetsScanArgs, AssetsStatusArgs, AssetsVerifyArgs,
};
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};

use dbmd_core::{assets, Store};

use super::projection::{load as load_projection, load_manifest as load_projection_manifest};

/// Dispatch `dbmd assets <sub>` to the matching leaf body.
pub fn run(ctx: &Context, args: &AssetsArgs) -> CliResult {
    match &args.command {
        AssetsCommand::Scan(a) => run_scan(ctx, a),
        AssetsCommand::Refresh(a) => run_refresh(ctx, a),
        AssetsCommand::RefreshWrapper(a) => run_refresh_wrapper(ctx, a),
        AssetsCommand::Verify(a) => run_verify(ctx, a),
        AssetsCommand::Status(a) => run_status(ctx, a),
        AssetsCommand::Paths(a) => run_paths(ctx, a),
    }
}

/// `dbmd assets refresh-wrapper <wrapper>` — bounded write-through for one
/// wrapper's complete current set, including an empty generated set.
fn run_refresh_wrapper(ctx: &Context, args: &AssetsRefreshWrapperArgs) -> CliResult {
    let store = Store::open_strict(Path::new(&args.dir))?;
    let _transaction = store
        .transaction()
        .map_err(|error| CliError::runtime(format!("cannot lock store transaction: {error}")))?;
    let report = assets::refresh_wrapper(&store, &args.wrapper)?;

    if ctx.json {
        println!(
            "{}",
            serde_json::to_string(&report).expect("refresh-wrapper report serializes")
        );
    } else {
        println!(
            "{} · {} cataloged · {} added · {} removed · {} hashed · {} preserved{}",
            report.wrapper,
            report.cataloged,
            report.added,
            report.removed,
            report.hashed,
            report.preserved,
            if report.wrote {
                " · manifest updated"
            } else {
                " · no change"
            }
        );
    }
    Ok(())
}

/// `dbmd assets refresh <path> --wrapper <wrapper>` — bounded write-through
/// for one newly created or deliberately changed asset.
fn run_refresh(ctx: &Context, args: &AssetsRefreshArgs) -> CliResult {
    let store = Store::open_strict(Path::new(&args.dir))?;
    let _transaction = store
        .transaction()
        .map_err(|error| CliError::runtime(format!("cannot lock store transaction: {error}")))?;
    let report = assets::refresh(&store, &args.path, &args.wrapper)?;

    if ctx.json {
        println!(
            "{}",
            serde_json::to_string(&report).expect("refresh report serializes")
        );
    } else {
        println!(
            "{} · {} bytes · {} wrapper(s){}",
            report.path,
            report.bytes,
            report.wrappers.len(),
            if report.wrote {
                " · manifest updated"
            } else {
                " · no change"
            }
        );
    }
    Ok(())
}

/// `dbmd assets scan` — rebuild the manifest from wrapper declarations.
fn run_scan(ctx: &Context, args: &AssetsScanArgs) -> CliResult {
    let store = Store::open_strict(Path::new(&args.dir))?;
    let _transaction = if args.dry_run {
        None
    } else {
        Some(store.transaction()?)
    };
    let report = assets::scan(&store, args.dry_run, args.untracked)?;

    if ctx.json {
        println!(
            "{}",
            serde_json::to_string(&report).expect("scan report serializes")
        );
    } else {
        let tail = if report.dry_run {
            " · (dry run, not written)"
        } else if report.wrote {
            " · manifest updated"
        } else {
            " · no change"
        };
        println!(
            "{} cataloged · {} hashed · {} preserved · {} bytes{tail}",
            report.cataloged, report.hashed, report.preserved, report.bytes
        );
        for w in &report.warnings {
            println!("warning: {w}");
        }
        for u in &report.untracked {
            println!("untracked: {u}");
        }
    }
    Ok(())
}

/// `dbmd assets verify` — the byte-completeness gate. Exits non-zero when any
/// required (or, with `--include-optional`, optional) asset is missing or
/// corrupt.
fn run_verify(ctx: &Context, args: &AssetsVerifyArgs) -> CliResult {
    let store = Store::open_strict(Path::new(&args.dir))?;
    let projection = match (
        args.projection_excludes.as_deref(),
        args.projection_manifest.as_deref(),
    ) {
        (Some(path), None) => Some(load_projection(&store, path)?),
        (None, Some(path)) => Some(load_projection_manifest(&store, path)?),
        (None, None) => None,
        (Some(_), Some(_)) => unreachable!("clap rejects conflicting projection inputs"),
    };
    let report = match projection.as_ref() {
        Some(excludes) => {
            assets::verify_projection(&store, args.include_optional, args.quick, excludes)?
        }
        None => assets::verify(&store, args.include_optional, args.quick)?,
    };

    if ctx.json {
        println!(
            "{}",
            serde_json::to_string(&report).expect("verify report serializes")
        );
    } else {
        println!(
            "{} checked · {} ok · {} missing · {} corrupt · {} projection-unresolved ({} mode)",
            report.checked,
            report.ok,
            report.missing.len(),
            report.corrupt.len(),
            report.projected_missing.len(),
            report.mode
        );
        for m in &report.missing {
            println!("missing: {m}");
        }
        for c in &report.corrupt {
            println!("corrupt: {c}");
        }
        for p in &report.projected_missing {
            println!("projection-unresolved: {p}");
        }
        println!(
            "{}",
            if report.complete {
                "PASS — byte-complete"
            } else if report.projection_complete == Some(true) {
                "PASS — materialized projection is byte-complete; excluded bytes remain unresolved"
            } else {
                "FAIL — store is not byte-complete"
            }
        );
    }

    if !report.projection_complete.unwrap_or(report.complete) {
        return Err(CliError::new(
            ExitCode::Runtime,
            "ASSET_INCOMPLETE",
            format!(
                "{} missing, {} corrupt",
                report.missing.len(),
                report.corrupt.len()
            ),
        )
        .with_hint("restore the bytes via your asset transport or sync, then re-verify"));
    }
    Ok(())
}

/// `dbmd assets status` — non-failing present/missing report.
fn run_status(ctx: &Context, args: &AssetsStatusArgs) -> CliResult {
    let store = Store::open_strict(Path::new(&args.dir))?;
    let report = assets::status(&store)?;

    if ctx.json {
        println!(
            "{}",
            serde_json::to_string(&report).expect("status report serializes")
        );
    } else {
        println!(
            "{} cataloged · {} present · {} missing ({} required, {} optional) · {} of {} bytes to restore",
            report.total,
            report.present,
            report.missing,
            report.required_missing,
            report.optional_missing,
            report.bytes_missing,
            report.bytes_total
        );
    }
    Ok(())
}

/// `dbmd assets paths` — the VCS-neutral path list.
fn run_paths(ctx: &Context, args: &AssetsPathsArgs) -> CliResult {
    let store = Store::open_strict(Path::new(&args.dir))?;
    let paths = assets::paths(&store)?;

    if ctx.json {
        println!(
            "{}",
            serde_json::to_string(&paths).expect("paths serialize")
        );
    } else {
        for p in &paths {
            println!("{p}");
        }
    }
    Ok(())
}
