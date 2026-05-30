---
type: wiki-page
id: flow-form-list
created: 2026-05-11T09:00:00-07:00
updated: 2026-05-11T09:00:00-07:00
summary: "Wiki page whose derived_from list uses the rejected flow form"
topic: Flow-form synthesis
derived_from: [[[records/companies/northstar]], [[records/contacts/sarah-chen]]]
tags: [synthesis]
status: active
---

# Flow-form synthesis

The derived_from field uses the inline flow form
[[[records/companies/northstar]], [[records/contacts/sarah-chen]]],
which YAML parses as a nested list rather than a list of wiki-link
strings. The spec requires the YAML block-sequence form, so this is the
flow-form-list error. The two referenced files both exist, so there is
no broken-link issue underneath it.
