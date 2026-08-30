# Security

db.md is an open file format. `dbmd` is its deterministic reference toolkit
and also carries the optional link.md network client. The format itself does
not provide encryption or access control. The link.md verbs do handle network
credentials, signing keys, untrusted hub responses, and replicated content.

## Reporting a vulnerability

Report privately with GitHub's **Report a vulnerability** button on this
repository's Security tab. Include affected versions, reproduction steps, and
impact. Do not open a public issue for an unpatched vulnerability.

## Local-store boundary

- Anyone who can read a store's files can read its data. Anyone who can write
  them can change its data. Use operating-system permissions and disk
  encryption for confidentiality.
- An opened store retains its root directory descriptor. Security-sensitive
  reads and writes traverse from that capability with `openat`, `O_NOFOLLOW`,
  bounded regular-file reads, and handle-relative atomic replacement; a later
  pathname or symlink swap cannot redirect them. Walks prune symlinks and
  nested-store markers.
- Parser and extractor input, aggregate inflation, XML-event, spine, table,
  cell, response, pack, and file-count budgets reject known amplification
  shapes before allocation. Document parser libraries run in a child process
  with a hard Unix CPU limit, a hard address-space limit where the kernel
  supports lowering it, and a parent-enforced 10 ms resident-memory watchdog
  on macOS. The parent also enforces an elapsed deadline and bounded transport.
  Extraction fails closed on native platforms where these process limits are
  unavailable.
- Rename stages every authored byte and persists a forward-recovery journal
  before commit. It installs the new target while the old target remains valid,
  commits all linkers, then removes the old target; restart recovery is
  idempotent and a destination race never clobbers an existing file.
- `dbmd` runs no model of its own and calls no provider on its own. Prompt
  injection and data disclosure by the agent that invokes `dbmd` remain
  properties of that agent harness.
- The embedded harness (`dbmd ask` / `do` / `build`) is the one surface that
  calls a model, only when invoked, only to the endpoint the operator
  configured, and never with a credential read from inside a store (see
  "Harness boundary" below).

## Harness boundary

`dbmd ask` / `do` / `build` run a tool-calling loop whose tools are `dbmd`
verbs. The boundary is the tool registry, not the prompt:

- **`ask` has no write tools at all**, so content injected through an
  ingested source can at worst produce a wrong answer.
- **`do` writes only through the same verbs a person would run**, so schema
  enforcement, frozen pages, link-aware deletes, the store transaction lock,
  and the append-only log all still apply, and every action is logged.
- **`build` adds file operations confined beneath a workspace root the
  operator declares** — absolute paths, `..`, symlinked components, and the
  store subtree are all refused. It is CLI-only and never served over HTTP.
- **There is no shell tool at any level**, and record content is passed to
  the model as data, never folded into the system prompt.
- **Credentials never come from a store.** API keys are read from the
  environment only; ChatGPT subscription tokens live in the toolkit state
  directory at 0600, and an Anthropic OAuth session is not copied here at all
  — it stays owned by Anthropic's `ant` CLI, which is asked for a fresh
  short-lived token per request. An endpoint selected by a store's own
  `.dbmd/config` cannot borrow an ambient key unless the operator explicitly
  binds that key to that origin, so a cloned store cannot exfiltrate one.
- **`.dbmd/config` cannot redirect a credential or a request.** The harness
  keys it may set are non-secret knobs only (provider, base URL, protocol,
  model, effort); a key line there is never read.
- **The `/v1/ask` and `/v1/do` routes are off unless started explicitly**
  (`dbmd api --ask` / `--do`), because they let anything on loopback spend
  the configured model's tokens.

## link.md boundary

Network access occurs only when an operator explicitly invokes a link.md verb:
`resolve`, `sync`, `grant`, `propose`, `subscribe`, `mirror`, or key rotation.
There is no telemetry or background phone-home behavior.

- A hub must be HTTPS, except for literal loopback development endpoints.
  Redirects are not followed while credentials or packs are in flight.
- `DBMD_HUB_KEY`, `DBMD_AGENT_KEY_FILE`, and `DBMD_BRAIN_KEY_FILE` are ambient
  credentials. If an untrusted store selects its hub through `.dbmd/config`,
  dbmd refuses to send a bearer, agent proof, brain signature, or brain
  content unless `DBMD_HUB_CREDENTIAL_ORIGIN` exactly binds it to that origin.
  Selecting the hub with `--hub` or `DBMD_HUB_URL` is explicit.
- Agent and brain key files must be regular, non-symlink files. On Unix they
  must not be group- or world-accessible. New keys are created atomically at
  mode 0600 and synced before their public identity can be sent remotely.
- Identity is trust-on-first-use. Pins and monotonic feed checkpoints live in
  the user's global state directory, never inside a cloned store. Override
  that directory only with an absolute `DBMD_STATE_DIR`.
- A later identity is accepted only through exact old-key-signed rotation
  statements. Each statement commits the prior feed sequence and hash, which
  bounds the old and new signing epochs. Feed rollback, same-sequence
  equivocation, broken hash chains, signer regression, and truncated/forked
  rotation histories are refused.
- Pull and mirror bind the export to the verified `(headSeq, feedHash)` token.
  Pack bytes must match the signed head's `pack_sha256`; full signed manifests
  are checked path-for-path and byte-for-byte. A path-scoped unsigned slice is
  refused until the protocol supplies an independently signed scoped proof.
- Registry-provided foreign homes are HTTPS origins whose DNS is resolved
  once, rejected if any answer is non-public, and pinned into a no-redirect
  client to prevent private-network SSRF and rebinding. Local federation tests
  require the explicit `DBMD_ALLOW_PRIVATE_REGISTRY_HOME=1` opt-in.
- Mirror staging, atomic exchange, cleanup, and pull materialization stay
  relative to held no-follow directory capabilities. `dbmd serve` no-follow
  reads bounded mirror files and re-verifies the identity chain, every feed
  signature/hash, rotation epoch, head, and snapshot pack before listening.
- TOFU cannot detect a malicious identity on the first connection. Verify an
  initial fingerprint out of band when that risk matters. A compromised
  current private key remains authoritative until a valid rotation or recovery
  changes it.

## Supply chain

The normal installer resolves both the latest version and its artifact digest
from the independently deployed Sevra release manifest. It then downloads the
GitHub release asset and verifies the independent SHA-256 before extraction.
The same-origin checksum mode is restricted to explicitly configured,
non-GitHub mirrors for controlled tests. GitHub releases are assembled as
drafts and receive every asset and provenance attestation. Before the protected
publishing environment is approved, the trusted local controller independently
rebuilds both Darwin binaries and both musl binaries and requires byte-for-byte
equality with CI. Linux builds use digest-pinned `cross` images; Darwin build
metadata is normalized before comparison. CI publishes the two permanent
crates first, then the immutable GitHub release without promoting it to latest.
The controller verifies exact crates.io package checksums, updates Homebrew
through an optimistic GitHub Contents API write using its existing `gh`
identity, and makes the GitHub release latest only after every channel
converges. CI stores no Homebrew deploy key or release write secret.

Release builds also publish Sigstore/GitHub build provenance. For the strongest
verification, download an asset and run:

```sh
gh attestation verify dbmd-*.tar.gz --repo carloslfu/db.md
```

The Rust dependency tree is permissive-only and contains no AI, embedding, or
vector dependencies. CI enforces this with `cargo deny`, RustSec advisories,
the shipped-closure license test, formatting, tests, and clippy. See
[`THIRD_PARTY_NOTICES`](THIRD_PARTY_NOTICES) and [`deny.toml`](deny.toml).

## Out of scope

- Encryption, backup policy, PII redaction, and filesystem access control.
- Trustworthiness of an agent or human authorized to change store content.
- Availability of a remote hub, independent release-manifest service, DNS,
  certificate authorities, or the operating system.
- Native Windows filesystem hardening. Official prebuilt installation supports
  Windows through WSL; native Windows builds are not a release target.
  Security-sensitive operations that persist identity checkpoints, create or
  rotate keys, pull snapshots, or mirror brains fail closed on native Windows
  instead of falling back to weaker path-based writes.
