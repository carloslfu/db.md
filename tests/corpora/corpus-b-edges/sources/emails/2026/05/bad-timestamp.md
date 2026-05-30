---
type: email
id: bad-timestamp
created: not-a-real-timestamp
updated: 2026-05-23T09:00:00-07:00
summary: "Email whose created field is not a valid ISO-8601 timestamp"
from: vendor@example.com
to: sarah@acme.com
date: 2026-05-23T09:00:00-07:00
subject: "Malformed created field"
thread: thread-9a1b
tags: [vendor]
status: active
---

# Malformed created field

The created field is the literal not-a-real-timestamp, which is not
ISO-8601. Because email has no explicit DB.md schema, the generic
frontmatter timestamp check owns this and reports a bad-timestamp
error. The date and updated fields are valid, so created is the only
offender.
