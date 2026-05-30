---
type: contact
id: ambiguous-link-contact
created: 2026-05-08T09:00:00-07:00
updated: 2026-05-08T09:00:00-07:00
summary: "Contact whose body uses a short-form link matching two files"
name: Toni Vance
email: toni@acme.com
company: [[records/companies/northstar]]
role: Analyst
first_touch: 2026-05-08
last_touch: 2026-05-08
tags: [internal]
status: active
---

# Toni Vance

This body uses the bare-basename link [[northstar]]. Two files in the
store carry that basename: records/companies/northstar.md and
wiki/companies/northstar.md. Under the defensive short-form resolver a
bare target that matches two or more files is reported as ambiguous
(rather than the plain short-form error), because the resolver cannot
pick one. The company link above is the correct full-path form.
