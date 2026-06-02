# Security

`db.md` is a file-format spec + reference tooling (`dbmd`, a single
deterministic CLI). The threat model is correspondingly simple:
`dbmd` reads and writes markdown files on a local filesystem. It
makes **no network calls and no LLM/API calls** — it ships zero AI
and handles no API keys. The intelligence lives in the user's own
agent harness, which drives `dbmd`; the security properties of that
harness are the operator's responsibility, not `dbmd`'s.

## Reporting a vulnerability

Report privately via GitHub's **"Report a vulnerability"** button on
the repository's Security tab (Security advisories). Include details
and reproduction steps. Do not open a public issue for security
problems. We aim to acknowledge within 2 business days.

## Threat model (summary)

**The store is a directory of markdown files.** Anyone who can
read the directory can read the store. Anyone who can write the
directory can write the store. There is no encryption, no
authentication, no access control built into the format — those
live at the filesystem layer (Unix permissions, encrypted
filesystem, etc.).

**`dbmd` itself sends nothing anywhere.** It computes on local
files only — no telemetry, no provider calls, no key handling. If
an agent harness driving `dbmd` sends file contents to an LLM, that
data flow belongs to the harness; review its configuration.

**Frontmatter parsing uses `serde_norway`.** YAML deserialization has
historically been a source of exploits when parsing untrusted
input. `dbmd` deserializes into typed structures, not arbitrary
code paths, which limits the attack surface. If you parse db.md
files from an untrusted source, review the YAML before parsing.

**Store traversal is rooted at the store directory.** `dbmd` walks
the store filesystem (via the `ignore` crate) and resolves
wiki-links by path relative to the store root; it does not resolve
`..` outside the store. As with any tool, a store containing
symlinks that point outside the store can expose those targets to
processes that read the store — keep untrusted symlinks out of a
store you operate on.

## What's out of scope

- **Access control on the wiki layer.** db.md is filesystem-level.
- **Encryption of file contents.** Use an encrypted filesystem
  (LUKS, FileVault, etc.) if the store contains sensitive data.
- **PII redaction.** Operator's responsibility. The store's
  `DB.md ## Policies` section can declare which types or fields the
  agent should treat as sensitive.
- **LLM prompt injection.** `dbmd` runs no model, so it has no
  prompt to inject. When an agent harness curates a store, a
  malicious source file could try to steer that agent. That risk
  lives in the harness, not in `dbmd`; operators should review
  agent-generated wiki edits before treating them as authoritative.

## Supply chain

`dbmd` is a single Rust binary with a small, audited dependency
tree (every crate + its license is recorded in
[`THIRD_PARTY_NOTICES`](THIRD_PARTY_NOTICES); MIT/Apache/BSD/
Unlicense/MPL only, zero AI/LLM crates). Build from source to verify.
