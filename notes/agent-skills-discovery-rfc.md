# Agent Skills Discovery RFC research

Research date: 2026-11-05
Researched for: T043 (read-only well-known discovery), continued by T044-T047.

## Sources

- Cloudflare RFC repository: <https://github.com/cloudflare/agent-skills-discovery-rfc>
- Version 0.2.0 text (branch `v0.2.0`):
  <https://raw.githubusercontent.com/cloudflare/agent-skills-discovery-rfc/v0.2.0/README.md>
- Version 0.1.0 text (branch `main`):
  <https://raw.githubusercontent.com/cloudflare/agent-skills-discovery-rfc/main/README.md>
- Adopters and write-ups:
  - <https://evilmartians.com/chronicles/publishing-agent-skills-discovery-index>
  - <https://www.mintlify.com/blog/skills-discovery-from-any-url>
  - <https://blog.cloudflare.com/agent-readiness>
  - <https://www.russ.cloud/2026/04/19/is-my-blog-agent-ready>

## Verified findings (version 0.2.0, branch `v0.2.0`)

- The index lives at `/.well-known/agent-skills/index.json`. This differs from
  version 0.1.0 on branch `main`, which used `/.well-known/skills/index.json`
  with a `files` array and no digests. The two versions are incompatible.
- The schema URI is an opaque identifier:
  `https://schemas.agentskills.io/discovery/0.2.0/schema.json`. The host
  `schemas.agentskills.io` does not resolve; it is an identifier, not a
  downloadable document (verified 2026-11-05 from this machine).
- Top-level fields: `$schema` (required) and `skills` (required array).
  `{"$schema": "...", "skills": []}` is valid.
- Missing `$schema` means the index is treated as version 0.1.0. An
  unrecognized `$schema` URI means the client should warn and not process the
  index.
- Entry fields, all required: `name`, `type`, `description`, `url`, `digest`.
- `name`: 1-64 characters, `a-z`, `0-9`, `-`; no leading, trailing, or
  consecutive hyphens. This matches the `SKILL.md` name rule already enforced
  by `src/skill.rs`, so `skillctl` shares one validation function.
- `type`: `skill-md` (one `SKILL.md` file) or `archive`. Unrecognized values
  are skipped with a warning.
- `url` resolution follows RFC 3986 section 5 with the index URL as the base:
  path-absolute URLs resolve against the origin, relative URLs resolve against
  the index URL directory, absolute URLs are used as declared.
- `digest`: `sha256:{hex}` with 64 lowercase hex characters, hashing the raw
  artifact bytes. Clients must verify downloaded content and must not use
  unverified content.
- Clients must handle 3xx redirects and must ignore unrecognized fields.
- Servers must answer 404 for missing skills and serve the index as
  `application/json`.

## Verified against a live publisher (2026-11-05)

- `https://www.russ.cloud` publishes a v0.2.0 index with one `skill-md` entry.
  `skillctl discover https://www.russ.cloud` resolves it correctly.
- The declared digest in that index did not match the bytes served for the
  entry's `SKILL.md` on the check date. The index is stale relative to the
  artifact. Read-only discovery reports the declared digest unchanged; digest
  enforcement belongs to T044 artifact verification.

Re-checked 2026-11-07, after T044 landed its digest enforcement:

- The index declares `sha256:bd9b7c88eeee83cd…503a30d74` for
  `read-blog-content`; the bytes served at that URL hash to
  `sha256:3d59eebc5f6b102c…f29dfcde`. The index is still stale, so acquiring
  this skill with the T044 path refuses it with a digest mismatch instead of
  staging the bytes.

## Open questions

- Version 0.2.0 lives on a branch while `main` still documents 0.1.0. Track
  which version is merged before T045 records well-known sources in lockfiles.
- The RFC may make `url` optional for `skill-md` entries with a conventional
  `/.well-known/agent-skills/{name}/SKILL.md` default. Not supported here.
- Origin trust landed with T046 (2026-11-07); the verified publisher list is
  short and grows only from hand-verified entries.

## Artifact acquisition as implemented (T044, 2026-11-07)

- Digest enforcement is now real. `skillctl::discovery::fetch_artifact`
  downloads the artifact an entry describes, hashes the bytes, and refuses a
  mismatch as a validation failure (exit 3) that names both digests. The bytes
  are discarded, never staged. The stale index on `https://www.russ.cloud`
  above would now be refused at acquisition time by design.
- The verified digest doubles as the staged revision, the way a commit does for
  a Git source. `crate::bundle::is_canonical_digest` already recognizes a
  `sha256:` digest as an immutable pin, so a later well-known import stays
  honest in update planning without a new revision model.
- Remote archives are unpacked in process (`src/source.rs`), never through
  `tar` or `unzip`. Detection is content based, because artifact URLs do not
  have to carry an extension.
- Formats accepted for a remote artifact: zip (deflate), tar, and gzip-
  compressed tar. bzip2 and xz are refused with a message that names the
  supported formats. The local archive install path still accepts them through
  the external tools. Moving that path onto the in-process unpacker would drop
  the shell dependency, but it is not required by T044.
- Rejection rules, each naming the offending entry: absolute names, `..`
  components, backslashes and colons (path separators and drive or stream
  markers on Windows), reserved Windows device names, empty or non-UTF-8
  names, symbolic and hard links, non-file non-directory entries, more than
  4096 entries, more than 64 MiB in one entry, more than 256 MiB unpacked
  total, and more than 64 MiB downloaded. A rejected archive leaves its
  extraction directory empty again.
- Archive permission bits are dropped while unpacking, so remote content
  arrives non-executable and the existing trust gate stays the only path that
  lets executable content in.
- A `skill-md` artifact is refused when the manifest inside declares a
  different name than the index entry, because the index name is what a later
  install pins.
- Staging routes archives through `normalized_extraction_root` and the
  direct-skill packaging path, so a bare skill archive and a wrapped release
  archive detect the same candidates as an equivalent local directory source.

## Managed install as implemented (T045, 2026-11-07)

- `skillctl install` accepts a well-known source only as an explicit `http://`
  or `https://` URL whose path is the origin root or whose final segment ends
  in `.json`. Every other URL keeps the Git interpretation, and a bare host
  keeps the existing Git-or-local diagnosis, so `install example.com` does not
  silently reach the network. `src/source.rs::well_known_target` holds the
  rule.
- The index entry is selected before anything is downloaded: `--name` picks
  the entry, an unknown name is a validation failure that lists the declared
  names, and a run without a selector refuses non-interactively with exit 5
  (an interactive run prompts over the declared entries). Only the selected
  entry's artifact is fetched.
- `SourceKind::WellKnown` and `ImportSourceType::WellKnown` serialize as
  `well-known`, and `install_records.source_kind` accepts the value after the
  state schema moved to version 6 (the additive migration rebuilds
  `install_records`, which carries no secondary index).
- The lockfile records the selected entry in a `discovery` block on the
  import: `skill`, `type`, `url`, `digest`. Validation requires the block on a
  well-known import, forbids it on every other kind, requires `skill` to equal
  the import id, and requires `digest` to equal `revision.resolved`. The
  manifest `ref` is the verified digest, so the pin names immutable content.
- Restore can re-fetch: `acquire_locked_source` rebuilds the artifact from the
  recorded URL, verifies it against the pinned digest, and refuses any bytes
  that hash differently. A fresh checkout without the import cache or the
  artifact store therefore restores from the origin instead of failing.
- Interim lifecycle behavior until T046 lands: `update` reports a
  `local-source` outcome with a note that says the index is not re-read, and
  telemetry classifies a discovery origin as `suppressed-private` because no
  origin trust policy exists yet. `pin` and `rollback` stay Git-only, which is
  what T046 asks for anyway.
- Still open for T046: re-reading the index to detect digest changes, origin
  trust, and a deliberate telemetry classification for public origins.

## Lifecycle as implemented (T046, 2026-11-07)

- `update` re-reads the discovery index for a well-known import. The check
  finds the entry by the skill name the lockfile's discovery block records,
  compares the declared digest with the pin, and reports `update-available`
  (digest differs), `up-to-date` (digest matches), or `failed` (index
  unreachable, entry dropped, or origin refused). The interim `local-source`
  outcome and its "this build does not re-read the discovery index" note are
  gone; `local-source` now covers folder and archive imports only.
- `update <skill> --apply` moves a well-known import. The engine re-reads the
  index, requires the entry it now declares to still name the digest the check
  saw, fetches and verifies the artifact, and applies through the shared
  guarded engine. The lockfile's discovery block, the pinned revision, and the
  manifest `ref` all move to the new digest, and no upstream revision is
  recorded, because a digest names no ref. A plan document whose digest the
  index moved past is refused with a note that names the newer digest and the
  re-run that observes it.
- Origin-trust policy (`src/trust.rs::discovery_origin_trust`): loopback
  origins are trusted for local indexes; HTTPS origins on the verified
  publisher list are trusted as public; other HTTPS origins are
  unverified-remote, which acquires with a warning and stays suppressed in
  telemetry; plain HTTP off loopback is refused, because an attacker on the
  network can serve an index whose artifacts hash to the digests the attacker
  declared, so the digest pin protects nothing. The policy gates `discover`,
  `install`, the update check, the update apply, and a frozen restore.
- Verified public discovery publishers: `www.russ.cloud`, verified against the
  RFC on 2026-11-05 and re-checked 2026-11-07 (its index is stale relative to
  its artifact, which acquisition refuses by design; telemetry classification
  does not fetch anything).
- Telemetry classification (`src/telemetry.rs`) is deliberate now: a loopback
  discovery origin is `suppressed-local`, a verified public publisher is
  `public`, and every other remote origin is `suppressed-private`.
- `pin` and `rollback` stay Git-only, as the task requires: a discovery index
  declares no branch or tag, and the digest pin every well-known install
  records is immutable by construction.

## MCP exposure and the publishing decision (T047, 2026-11-08)

- The MCP server ships `skills_discover`. It takes one required `url`
  argument and dispatches through the same `runtime::execute_command` path as
  the CLI, so the `structuredContent` is byte-for-byte the envelope
  `skillctl discover <url> --json` prints. `tests/mcp.rs::
  mcp_discover_matches_the_cli_json_response` holds the parity for a clean
  index (exit 0), a skipped-entry index (exit 2, warning carried), and an
  invalid-digest index (exit 3, `isError` with the same error text).
- Documented tool lists that the drift test enforces now carry the tool:
  `src/cli.rs` `MCP_AFTER_LONG_HELP`, `docs/troubleshooting.md`, and
  `spec.md` section 22.2.
- Publishing support was evaluated and stays out. Reasons recorded 2026-11-08:
  the RFC's 0.2.0 schema lives on a branch while `main` still documents the
  incompatible 0.1.0 format, this project verified exactly one live publisher
  (and its index is stale relative to its artifact), and growing the verified
  publisher list is blocked on maintainer nominations (backlog T152).
  Generating an `index.json` now would couple the CLI to a draft format
  before either side is stable.
- Resume condition for a later publishing task: the RFC merges a stable
  schema version and several public publishers verify against it. That task
  must generate compliant `index.json` output from local content without
  coupling it to runtime projection, per the boundary `docs/user-guide.md`
  "The discovery boundary" records and the non-goal `spec.md` section 5 adds.
