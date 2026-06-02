#!/usr/bin/env python3
"""Generate the corpus-a index hierarchy as derived, write-through artifacts.

These files are NOT goldens. They are the store's own derived catalog — the
SPEC says index files are "derived, write-through, rebuildable" and "never
edited by hand". This generator mirrors the documented render rules in
`crates/dbmd-core/src/index.rs` so the in-store catalog is complete and
internally consistent (root + every layer + every non-empty type-folder, both
`index.md` and the complete `index.jsonl` twin), letting `dbmd validate --all`'s
INDEX_* checks pass.

The byte-identity audit (write-through == `dbmd index rebuild`) belongs to the
End-to-end phase per plans/db-md-rust-toolkit.md; this generator stands in until
the `dbmd` binary exists to regenerate and lock these files.

Render rules mirrored (from index.rs):
  - record_from_file: type/summary/tags/links from frontmatter; `links` ONLY
    from an explicit `links:` key (none here → []); every other key (id, status,
    type-specific) → `fields`; wiki-link-valued fields kept as raw "[[...]]".
  - to_jsonl: serde field order is path, type, summary, tags, links, created,
    updated, then the flattened `fields` in BTreeMap (sorted) key order.
  - sort_records: updated DESC, ties by store-relative path ASC.
  - type-folder index.md: frontmatter (type/scope/folder/updated=max-updated),
    `# <folder>`, one `- [[bare-path]] — summary  ·  #tag #tag` line per record
    (cap 500; this corpus stays under the cap so no `## More`).
  - layer index.md: type/scope/folder/updated, `# <layer>`, one
    `- [[tf/index|Cap]] (N) — <newest summary, ≤80 chars>` per non-empty folder
    (alphabetical), preview omitted if summary is the missing-placeholder.
  - root index.md: type/scope/updated, `# Knowledge base index`, a
    `## <Layer> (total)` heading per non-empty layer with `- [[tf/index|Cap]] (N)`
    children.
  - fmt_ts: RFC3339, `Z` only for a UTC (+00:00) offset; other offsets verbatim.

Run from the corpus root:  python3 .gen-index.py
"""

import json
import os
from datetime import datetime

ROOT = os.path.dirname(os.path.abspath(__file__))
RESERVED = {"type", "summary", "tags", "links", "created", "updated", "path"}
LAYERS = ["sources", "records", "wiki"]
MD_CAP = 500


def parse_frontmatter(path):
    """Minimal frontmatter parse mirroring index.rs::read_frontmatter via PyYAML
    if available, else a small hand parser for the flat YAML this corpus uses."""
    with open(path) as fh:
        text = fh.read()
    if not text.startswith("---\n"):
        return None
    end = text.index("\n---", 4)
    block = text[4:end + 1]
    try:
        data = _yaml_load_strings(block) or {}
    except Exception:
        data = _hand_yaml(block)
    return data


# A YAML loader that does NOT implicitly resolve dates/timestamps, mirroring
# serde_norway: an unquoted `2026-04-01` is a String, not a date type (serde_json
# then serializes it as a JSON string). PyYAML's SafeLoader otherwise coerces
# ISO dates to datetime.date / datetime.datetime, which (a) aren't
# JSON-serializable and (b) wouldn't match the Rust string form.
def _yaml_load_strings(block):
    import yaml  # type: ignore

    class _NoDatesLoader(yaml.SafeLoader):
        pass

    # Drop the implicit resolvers for timestamp (date/datetime) so those scalars
    # fall through to the default `str` resolution.
    new_resolvers = {}
    for ch, mappings in _NoDatesLoader.yaml_implicit_resolvers.items():
        kept = [(tag, regexp) for (tag, regexp) in mappings
                if tag != "tag:yaml.org,2002:timestamp"]
        new_resolvers[ch] = kept
    _NoDatesLoader.yaml_implicit_resolvers = new_resolvers
    return yaml.load(block, Loader=_NoDatesLoader)


def _hand_yaml(block):
    """Parse the restricted flat YAML this corpus emits: scalars, [..] inline
    lists, and `key:` followed by `  - item` block lists."""
    out = {}
    lines = block.splitlines()
    i = 0
    while i < len(lines):
        line = lines[i]
        if not line.strip() or line.lstrip().startswith("#"):
            i += 1
            continue
        if line.startswith("  ") or line.lstrip().startswith("- "):
            i += 1
            continue
        key, _, rest = line.partition(":")
        key = key.strip()
        rest = rest.strip()
        if rest == "":
            # block list follows?
            items = []
            j = i + 1
            while j < len(lines) and (lines[j].lstrip().startswith("- ")):
                items.append(lines[j].lstrip()[2:].strip().strip('"'))
                j += 1
            if items:
                out[key] = items
                i = j
                continue
            out[key] = None
            i += 1
            continue
        if rest.startswith("[") and rest.endswith("]"):
            inner = rest[1:-1].strip()
            out[key] = [x.strip().strip('"') for x in inner.split(",")] if inner else []
        else:
            out[key] = rest.strip('"')
        i += 1
    return out


def fmt_ts(s):
    """RFC3339 normalize mirroring fmt_ts: `Z` only for +00:00; AutoSi drops a
    zero sub-second. Our corpus timestamps are already canonical (no subseconds,
    explicit offset), so they pass through unchanged except a +00:00 → Z."""
    if s is None:
        return None
    s = str(s)
    if s.endswith("+00:00"):
        return s[:-6] + "Z"
    return s


def ts_key(s):
    """Sort key from an RFC3339 string; None sorts last."""
    if not s:
        return (1, datetime.min.replace(tzinfo=None))
    iso = s.replace("Z", "+00:00")
    return (0, datetime.fromisoformat(iso).astimezone().replace(tzinfo=None) * -1
            if False else datetime.fromisoformat(iso).timestamp())


def record_from_file(abs_path, rel):
    fm = parse_frontmatter(abs_path) or {}
    fields = {}
    for k, v in fm.items():
        if k in RESERVED:
            continue
        fields[k] = v
    return {
        "path": rel,
        "type": fm.get("type") or "",
        "summary": fm.get("summary") or "(no summary)",
        "tags": fm.get("tags") or [],
        "links": fm.get("links") or [],
        "created": fmt_ts(fm.get("created")),
        "updated": fmt_ts(fm.get("updated")),
        "fields": fields,
    }


def sort_records(records):
    # updated DESC (None last), ties by path ASC.
    def key(r):
        u = r["updated"]
        if u is None:
            return (1, 0.0, r["path"])
        iso = u.replace("Z", "+00:00")
        # negative timestamp → descending
        return (0, -datetime.fromisoformat(iso).timestamp(), r["path"])
    records.sort(key=key)


def jsonl_line(rec):
    """serde_json order: path, type, summary, tags, links, created, updated,
    then flattened fields in sorted (BTreeMap) key order. Compact separators."""
    ordered = {
        "path": rec["path"],
        "type": rec["type"],
        "summary": rec["summary"],
        "tags": rec["tags"],
        "links": rec["links"],
        "created": rec["created"],
        "updated": rec["updated"],
    }
    for k in sorted(rec["fields"].keys()):
        ordered[k] = rec["fields"][k]
    return json.dumps(ordered, ensure_ascii=False, separators=(",", ":"))


def wiki_target(rel):
    return rel[:-3] if rel.endswith(".md") else rel


def md_entry(rec):
    line = f"- [[{wiki_target(rec['path'])}]] — {rec['summary']}"
    if rec["tags"]:
        tags = " ".join(f"#{t}" for t in rec["tags"])
        line += f"  ·  {tags}"
    return line


def capitalize(s):
    return s[:1].upper() + s[1:] if s else s


def type_folder_files(folder_abs):
    out = []
    for dirpath, dirnames, filenames in os.walk(folder_abs):
        dirnames[:] = [d for d in dirnames if not d.startswith(".")]
        for fn in filenames:
            if fn == "index.md" or not fn.endswith(".md"):
                continue
            out.append(os.path.join(dirpath, fn))
    return out


def rel_of(abs_path):
    return os.path.relpath(abs_path, ROOT).replace(os.sep, "/")


def build_type_folder(tf_rel):
    abs_folder = os.path.join(ROOT, tf_rel)
    records = [record_from_file(f, rel_of(f)) for f in type_folder_files(abs_folder)]
    sort_records(records)
    return records


def write_type_folder(tf_rel, records):
    abs_folder = os.path.join(ROOT, tf_rel)
    # index.jsonl — complete, one line per record, canonical order.
    jsonl = "".join(jsonl_line(r) + "\n" for r in records)
    with open(os.path.join(abs_folder, "index.jsonl"), "w") as fh:
        fh.write(jsonl)
    # index.md — capped browse view (under cap here).
    max_upd = next((r["updated"] for r in records if r["updated"]), None)
    s = ["---", "type: index", "scope: type-folder", f"folder: {tf_rel}"]
    if max_upd:
        s.append(f"updated: {max_upd}")
    s.append("---")
    s.append("")
    s.append(f"# {tf_rel}")
    s.append("")
    for r in records[:MD_CAP]:
        s.append(md_entry(r))
    md = "\n".join(s) + "\n"
    with open(os.path.join(abs_folder, "index.md"), "w") as fh:
        fh.write(md)


def truncate(s, n):
    return s if len(s) <= n else s[:n]


def nonempty_type_folders(layer):
    layer_abs = os.path.join(ROOT, layer)
    out = []
    for name in sorted(os.listdir(layer_abs)):
        p = os.path.join(layer_abs, name)
        if not os.path.isdir(p) or name.startswith("."):
            continue
        if type_folder_files(p):
            out.append(f"{layer}/{name}")
    return out


def child_count(tf_rel):
    return len(type_folder_files(os.path.join(ROOT, tf_rel)))


def newest_summary_and_updated(tf_rel):
    recs = build_type_folder(tf_rel)
    if not recs:
        return (None, None)
    return (recs[0]["summary"], recs[0]["updated"])


def write_layer(layer):
    tfs = nonempty_type_folders(layer)
    if not tfs:
        return False
    max_upd = None
    entries = []
    for tf in tfs:
        n = child_count(tf)
        summary, upd = newest_summary_and_updated(tf)
        if upd and (max_upd is None or upd > max_upd):
            max_upd = upd
        disp = capitalize(tf.split("/")[-1])
        preview = truncate(summary, 80) if summary and summary != "(no summary)" else None
        if preview:
            entries.append(f"- [[{tf}/index|{disp}]] ({n}) — {preview}")
        else:
            entries.append(f"- [[{tf}/index|{disp}]] ({n})")
    s = ["---", "type: index", "scope: layer", f"folder: {layer}"]
    if max_upd:
        s.append(f"updated: {max_upd}")
    s.append("---")
    s.append("")
    s.append(f"# {layer}")
    s.append("")
    s.extend(entries)
    with open(os.path.join(ROOT, layer, "index.md"), "w") as fh:
        fh.write("\n".join(s) + "\n")
    return True


def write_root():
    all_max = None
    blocks = []
    for layer in LAYERS:
        tfs = nonempty_type_folders(layer)
        if not tfs:
            continue
        total = sum(child_count(tf) for tf in tfs)
        for tf in tfs:
            _, upd = newest_summary_and_updated(tf)
            if upd and (all_max is None or upd > all_max):
                all_max = upd
        lines = [f"## {capitalize(layer)} ({total})"]
        for tf in tfs:
            n = child_count(tf)
            disp = capitalize(tf.split("/")[-1])
            lines.append(f"- [[{tf}/index|{disp}]] ({n})")
        blocks.append("\n".join(lines))
    s = ["---", "type: index", "scope: root"]
    if all_max:
        s.append(f"updated: {all_max}")
    s.append("---")
    s.append("")
    s.append("# Knowledge base index")
    out = "\n".join(s) + "\n"
    for b in blocks:
        out += "\n" + b + "\n"
    with open(os.path.join(ROOT, "index.md"), "w") as fh:
        fh.write(out)


def main():
    n_tf = 0
    for layer in LAYERS:
        for tf in nonempty_type_folders(layer):
            records = build_type_folder(tf)
            write_type_folder(tf, records)
            n_tf += 1
        write_layer(layer)
    write_root()
    print(f"wrote {n_tf} type-folder indexes + {len(LAYERS)} layer indexes + root index")


if __name__ == "__main__":
    main()
