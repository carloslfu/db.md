---
type: note
id: malformed-tags
created: 2026-03-03T09:00:00-08:00
updated: 2026-03-03T09:00:00-08:00
summary: "Note whose tags field is a nested YAML list, not a flat list of scalar labels"
tags:
  - [nested, list]
  - ok
status: active
---

# Malformed tags note

The frontmatter parses as valid YAML, so this is NOT `FM_MALFORMED_YAML`.
But `tags` is a sequence whose first element is itself a sequence
(`[nested, list]`), so it is not a flat list of scalar labels — the
single breakage here is `TAGS_MALFORMED`. Everything else (type,
summary, timestamps) is clean.
