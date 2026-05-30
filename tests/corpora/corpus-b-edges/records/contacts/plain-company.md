---
type: contact
id: dana-lee
created: 2026-05-16T09:00:00-07:00
updated: 2026-05-16T09:00:00-07:00
summary: "Contact whose company is a plain string, not a wiki-link"
name: Dana Lee
email: dana@acme.com
company: "Acme Inc"
role: Account Manager
first_touch: 2026-05-16
last_touch: 2026-05-16
tags: [internal]
status: active
---

# Dana Lee

The `company` field here is the plain string `"Acme Inc"` instead of a
wiki-link such as `records/companies/acme` written in double-bracket
form. The schema declares `company (required, link to
records/companies/)`, so this is a link prefix mismatch. (This sentence
deliberately does NOT use a real wiki-link, so the file's only issue is
the frontmatter one above.)
