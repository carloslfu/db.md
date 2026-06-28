//! Regression tests for `dbmd index` — owned by the core/CLI index group.
//!
//! Distinct filename (`regression_coreindex`) so it never collides with the
//! shared `tests/index.rs`. Each test reconstructs a confirmed launch-blocking
//! bug's exact trigger and asserts the corrected behavior through the real
//! `dbmd` binary, so the test would fail against the pre-fix code.

mod common;

use common::{copy_store_to_temp, corpus_a, dbmd};

/// Finding #14 — `index rebuild --layer` left the root `index.md` stale.
///
/// The `--layer` path rebuilt each type-folder sidecar and the layer `index.md`
/// but never re-rendered the root `index.md`, which embeds per-folder `(n)`
/// counts and per-layer totals derived from the sidecars. So a layer-scoped
/// repair that changed a folder's record count (the whole point of repairing a
/// damaged/stale index) corrected the folder + layer rollups but left the root
/// showing the OLD counts — the exact root/folder desync `rebuild_folder` was
/// written to avoid.
///
/// Trigger: a store where a folder's on-disk record set diverges from the root
/// rollup. corpus-a ships 4 contacts (`Contacts (4)`, layer total `Records
/// (509)`); we add a 5th contact file on disk WITHOUT touching any index, then
/// run `dbmd index rebuild --layer records`. Post-fix, the folder, layer, AND
/// root must all reflect 5 contacts / a `Records` total of 506. Pre-fix, the
/// folder + layer were corrected but the root still read `Contacts (4)` /
/// `Records (509)`.
#[test]
fn regression_rebuild_layer_refreshes_root_index_counts() {
    let (_tmp, store) = copy_store_to_temp(&corpus_a());

    // Precondition: corpus-a's committed root rollup reads the pre-divergence
    // counts (4 contacts, 505 in the records layer).
    let root_before = std::fs::read_to_string(store.join("index.md")).unwrap();
    assert!(
        root_before.contains("- [[records/contacts/index|Contacts]] (4)\n"),
        "precondition: corpus-a root must list 4 contacts:\n{root_before}"
    );
    assert!(
        root_before.contains("## Records (509)\n"),
        "precondition: corpus-a records layer total must be 505:\n{root_before}"
    );

    // Add a 5th contact file directly on disk — disk now diverges from every
    // committed index (folder, layer, and root all still say 4 / 505).
    std::fs::write(
        store.join("records/contacts/nadia-petrov.md"),
        "---\n\
         type: contact\n\
         created: 2026-05-30T09:00:00-07:00\n\
         updated: 2026-05-30T09:00:00-07:00\n\
         summary: \"New contact added on disk before indexing\"\n\
         name: Nadia Petrov\n\
         email: nadia.petrov@northstar.example\n\
         role: Operations Lead\n\
         tags: [customer]\n\
         status: active\n\
         ---\n\n\
         Body.\n",
    )
    .unwrap();

    // The documented repair tool, layer-scoped.
    dbmd()
        .current_dir(&store)
        .args(["index", "rebuild", "--layer", "records"])
        .assert()
        .success();

    // The type-folder + layer rollups are corrected (this part worked pre-fix —
    // asserting it proves the repair actually ran and reached the new state).
    let contacts_jsonl =
        std::fs::read_to_string(store.join("records/contacts/index.jsonl")).unwrap();
    assert!(
        contacts_jsonl.contains("records/contacts/nadia-petrov.md"),
        "folder sidecar must catalog the new contact:\n{contacts_jsonl}"
    );
    let records_layer = std::fs::read_to_string(store.join("records/index.md")).unwrap();
    assert!(
        records_layer.contains("- [[records/contacts/index|Contacts]] (5)\n"),
        "layer rollup must show 5 contacts:\n{records_layer}"
    );

    // THE regression: the root index.md must also be refreshed — pre-fix it
    // kept the stale 4 / 505.
    let root_after = std::fs::read_to_string(store.join("index.md")).unwrap();
    assert!(
        root_after.contains("- [[records/contacts/index|Contacts]] (5)\n"),
        "root index.md must reflect 5 contacts after `index rebuild --layer records`, \
         not the stale 4 (root/folder count desync):\n{root_after}"
    );
    assert!(
        root_after.contains("## Records (510)\n"),
        "root index.md records-layer total must be refreshed to 506 after the \
         layer rebuild, not the stale 505:\n{root_after}"
    );
    assert!(
        !root_after.contains("- [[records/contacts/index|Contacts]] (4)\n"),
        "stale `Contacts (4)` count must be gone from root:\n{root_after}"
    );
}
