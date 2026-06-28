---
type: project
meta-type: operational
created: 2026-04-12T16:00:00-07:00
updated: 2026-06-27T00:00:00-07:00
summary: "Self-hosting setup on the home server; Caddy reverse proxy + TLS done, off-site backup is the open item."
next_step: Set up off-site backup for the photo archive (boring + important)
items:
  - "[x] Stand up the home server (mini-PC, Ubuntu, ZFS pool)"
  - "[x] Run Jellyfin for media"
  - "[x] Photo backup with Immich"
  - "[x] Replace ad-hoc nginx with Caddy reverse proxy + real TLS"
  - "[ ] Get every service behind the proxy (Jellyfin + photos done) — in progress"
  - "[ ] Off-site backup of the photo archive (restic → object storage)"
  - "[ ] Uptime monitoring + alerting so I know when something dies"
tags: [project, homelab, self-hosting]
status: active
---

# Home lab

My self-hosting setup — the family media library, photo backups, and a
few small services, all on a mini-PC in the closet. This is a
`meta-type: operational` record: the curator flips the items below
between `[ ]` and `[x]` in place as the project moves (with an "in
progress" note on whatever's mid-flight), rather than appending new
data points.

## State as of the last update

The big recent win was tearing down the hand-rolled nginx config and
putting **Caddy** in front of everything, so services finally have real
TLS instead of the self-signed cert I'd been clicking through. Jellyfin
and the photo backup are behind it now.

The open blocker is the **off-site backup** — the one item I keep
deferring because it's boring and important at the same time, the worst
combination. Until that's done, a closet fire is a single point of
failure for the family photos, which is the whole reason this exists.

## Source

Last worked on over the weekend of 05-20; see
[[sources/journal/2026-05-20-mentor-call-and-homelab]].
