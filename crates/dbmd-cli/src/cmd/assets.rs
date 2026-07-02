//! `dbmd assets <sub>` — the heavy-binary asset manifest.
//!
//! Arg-parse + format glue only; all logic lives in [`dbmd_core::assets`].
//! Dispatches the four leaves:
//!   - `scan`   → discover declarations, hash present files, rewrite `assets.jsonl`
//!   - `verify` → byte-completeness gate (exits non-zero when incomplete)
//!   - `status` → present / missing report (never fails)
//!   - `paths`  → the store-relative path list (for an ignore mechanism)
//!
//! None of these runs git or touches the network. Keeping bytes out of a VCS is
//! the harness's job; `dbmd assets paths` is the VCS-neutral list it consumes.

use std::path::Path;

use crate::cli::{
    AssetsArgs, AssetsCommand, AssetsPathsArgs, AssetsScanArgs, AssetsStatusArgs, AssetsVerifyArgs,
};
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};

use dbmd_core::{assets, Store};

/// Dispatch `dbmd assets <sub>` to the matching leaf body.
pub fn run(ctx: &Context, args: &AssetsArgs) -> CliResult {
    match &args.command {
        AssetsCommand::Scan(a) => run_scan(ctx, a),
        AssetsCommand::Verify(a) => run_verify(ctx, a),
        AssetsCommand::Status(a) => run_status(ctx, a),
        AssetsCommand::Paths(a) => run_paths(ctx, a),
    }
}

/// `dbmd assets scan` — rebuild the manifest from wrapper declarations.
fn run_scan(ctx: &Context, args: &AssetsScanArgs) -> CliResult {
    let store = Store::open_strict(Path::new(&args.dir))?;
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
    let report = assets::verify(&store, args.include_optional, args.quick)?;

    if ctx.json {
        println!(
            "{}",
            serde_json::to_string(&report).expect("verify report serializes")
        );
    } else {
        println!(
            "{} checked · {} ok · {} missing · {} corrupt ({} mode)",
            report.checked,
            report.ok,
            report.missing.len(),
            report.corrupt.len(),
            report.mode
        );
        for m in &report.missing {
            println!("missing: {m}");
        }
        for c in &report.corrupt {
            println!("corrupt: {c}");
        }
        println!(
            "{}",
            if report.complete {
                "PASS — byte-complete"
            } else {
                "FAIL — store is not byte-complete"
            }
        );
    }

    if !report.complete {
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
