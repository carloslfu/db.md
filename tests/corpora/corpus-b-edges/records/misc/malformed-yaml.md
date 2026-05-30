---
type: contact
id: malformed-yaml
created: 2026-05-02T09:00:00-07:00
summary: "Frontmatter that is not valid YAML"
tags: [unclosed, list
name: "unterminated string
---

# Malformed YAML

The frontmatter block above is not parseable YAML: `tags` opens a flow
sequence that is never closed and `name` opens a double-quoted scalar
that is never terminated. The whole block fails to parse, so this is
`FM_MALFORMED_YAML` and no field-level checks run on it.
