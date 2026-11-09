# Canonical bundle format

Status: Active
Date: 2026-11-05
Updated: 2026-11-09, format version 2
Owner: `src/bundle.rs`, `src/artifact.rs`

This document specifies the canonical bundle format and hashing rules that
`skillctl` uses for every digest that gates an install, a restore, or an
approval decision. It implements the versioning and reproducibility contract in
`plans/001-product-and-development-roadmap.md`, section 9.

The format version is a compatibility boundary. Changing any rule in this
document changes digest values. A change requires a new format version and a
documented migration path, never a silent reinterpretation of an existing
digest.

## Why a canonical bundle exists

A Git commit plus a subpath identifies source content. A bundle digest
identifies the retained artifact that `skillctl` actually delivers. The two are
kept distinct:

| Identity | What it is | Where it is recorded |
| --- | --- | --- |
| Revision | Immutable Git commit, or archive byte digest | `imports.<id>.revision.resolved` |
| Canonical bundle digest | Digest of the versioned file inventory | `imports.<id>.bundle.digest` |
| Content hash | Legacy whole-tree hash of the skill subtree | `imports.<id>.hashes.content` |
| Overlay hash | Hash of the applied overlay set | `imports.<id>.hashes.overlay` |
| Effective version hash | Derived from revision, content, and overlay | `imports.<id>.hashes.effective_version` |

Archive byte digests and canonical content digests remain distinct. An archive
that is re-packed with the same content has a different byte digest and the
same canonical bundle digest.

## Scope of a bundle

A bundle covers one imported skill subtree: the selected subpath inside its
source, which is also the tree retained in the import cache. Every distributed
file is inventoried, not only `SKILL.md`. Instructions, references, assets,
scripts, and declared configuration are all covered.

Excluded from source identity:

- transport metadata, such as file modification times,
- generated local metadata, such as `.skillctl-projection.json`,
- the `.git` directory of a cloned source.

## Scope of an overlay

An overlay is not part of the bundle. The lockfile records only the digest of
the applied overlay set. The overlay files themselves are committable portable
state in the repository, next to `.agents/skillctl.yaml` and
`.agents/skillctl.lock`, so a checkout that omits `.agents/overlays/` cannot
reproduce a pinned overlay digest. `skillctl restore --frozen` refuses that
state and names the missing directory with its pinned digest.

## Path rules

Bundle paths are relative to the bundle root and:

- are valid UTF-8,
- use `/` as the only separator,
- have no empty, `.`, or `..` segments and no root segment,
- contain no backslash, colon, or question mark, and no control characters,
- do not end a segment with `.` or a space,
- do not use a Windows-reserved device name (`CON`, `PRN`, `AUX`, `NUL`,
  `COM1` through `COM9`, `LPT1` through `LPT9`),
- are at most 1024 bytes long, with segments of at most 255 bytes and at most
  32 segments deep.

A path is never normalized after approval. Line endings, Unicode form, and
content are hashed exactly as supplied. If a destination needs transformed
content, the transformation produces a distinct destination artifact.

## Entry rules

Each entry records a path, a mode, a digest, and a size.

- Modes are `file`, `executable`, and `directory`.
- `executable` means the file has at least one execute permission bit.
- The mode is part of the stored inventory only. Since format version 2, the
  digest payload hashes `file` and `executable` entries with the same
  `file` tag, because platforms without permission bits, such as Windows,
  cannot observe the bit. A tree therefore digests identically on every
  platform.
- Directories carry no digest and a size of zero. Explicit directory entries
  make empty directories portable.
- A file digest is the SHA-256 of the file bytes, rendered as
  `sha256:<64 lowercase hex>`.

Rejected during capture:

- symbolic links,
- regular files with more than one hard link,
- device nodes, FIFOs, and sockets,
- duplicate normalized paths,
- distinct paths that collide after case folding, including Unicode simple
  case folding, for example `Reference/a` and `reference/A`.

Rejection is the only outcome. Ambiguous or unsupported input is never silently
dropped from a bundle.

## Bounds

The default import bounds are:

| Bound | Default |
| --- | --- |
| Regular file entries | 4096 |
| Directory entries | 4096 |
| Total bytes | 64 MiB |
| Single file bytes | 32 MiB |
| Path depth | 32 segments |
| Path length | 1024 bytes |
| Segment length | 255 bytes |

A tree that exceeds a bound is rejected before its content is hashed or
trusted.

## Digest payload

The bundle digest is the SHA-256 of a deterministic payload:

1. A header line: `skillctl-bundle-v<format-version>` followed by a newline.
2. A count line: `entries:<entry count>` followed by a newline.
3. One line per entry, sorted by path bytes ascending:
   `path`, NUL, `mode`, NUL, `digest`, NUL, `size`, NUL, newline. Directories
   use an empty digest field and `0` as the size.

The `mode` field of the payload is a versioned tag, not the stored mode:

- Version 2, the current version, writes `file` for both `file` and
  `executable` entries and `directory` for directories.
- Version 1 wrote the stored mode, so `executable` entries carried an
  `executable` tag.

The digest is rendered as `sha256:<64 lowercase hex>`.

The committed fixture `tests/fixtures/bundle/canonical/` pins both values. A
current version 2 capture digests to:

```text
sha256:367c651e157b6b28646d3627d147b67b66da58bb5b0692ae3fabba35caaf937b
```

The same tree under the version 1 payload rules digests to:

```text
sha256:0766869ba27a516348be813c39a52a8ca862ce7338708117a1b0044e9d86bc0f
```

The assertion in `src/bundle.rs` is the compatibility boundary. Changing the
fixture or the rules changes the digest and requires a version bump.

### Format version migration

Version 1 manifests remain readable, and a version 1 manifest digests with the
version 1 payload, so retained artifacts stay verifiable under the digest they
recorded. A lockfile pin recorded under version 1 no longer matches a fresh
capture of the same tree, because a fresh capture records version 2. Reinstall
the import to record a version 2 pin.

## Stored manifest

A retained artifact stores the inventory as deterministic JSON at
`bundle.json`:

```json
{
  "format_version": 2,
  "entries": [
    {
      "path": "SKILL.md",
      "mode": "file",
      "digest": "sha256:...",
      "size": 136
    }
  ]
}
```

Entries are sorted by path. A stored manifest that is unsorted, carries an
unsupported format version, or contains a malformed digest is rejected on load.

## Immutable artifact storage

Each verified bundle is retained under the skillctl home directory:

```text
~/.skillctl/store/artifacts/<bundle-digest-hex>/
  bundle.json
  content/
```

The directory name is the bundle digest, so an artifact is portable across
workspaces and machines. It never references a workspace key, an access token,
or a local database.

Artifacts are write-once:

- Storing an artifact that already exists verifies it and leaves it untouched.
- An artifact that fails verification is reported as tampering and is never
  overwritten in place.
- New artifacts are staged beside the store and moved into place only after the
  staged copy verifies.

The artifact store is a cache, not dependency state. It may be evicted at any
time. Eviction changes how a restore reconstructs content, never what content
is acceptable.

## Portable lockfile

`.agents/skillctl.lock` records dependency identity only:

```yaml
version: 2
state:
  manifest_version: 1
  local_state_version: 6
imports:
  release-notes:
    source:
      type: git
      url: https://example.com/skills.git
      subpath: .agents/skills/release-notes
    revision:
      resolved: 0123456789abcdef0123456789abcdef01234567
    bundle:
      format_version: 2
      digest: sha256:...
      files: 3
      bytes: 1200
    hashes:
      content: sha256:...
      overlay: sha256:none
      effective_version: sha256:...
```

`revision.upstream` is optional and records the last upstream revision an
operation really observed. A move that follows a branch or tag observes the
revision it resolved to. A move to a commit, such as `skillctl pin <skill>
<commit>` or `skillctl rollback <skill> <commit>`, observes nothing upstream, so
the field is left out rather than filled with the revision that was moved to.

Not dependency identity, and therefore never recorded in the lockfile:
machine-specific cache paths, access tokens, device identifiers, local database
internals, and local timestamps such as fetch and install times. Those live in
the local state store.

### Version 1 compatibility

Version 1 lockfiles recorded per-import `timestamps`. They remain loadable
through a compatibility loader that upgrades them in memory:

- Reading a version 1 lockfile succeeds. Its timestamps are local bookkeeping
  and are dropped on load.
- `skillctl restore --frozen` leaves such a file byte-for-byte unchanged.
- `skillctl restore` upgrades the file to the current version, recording the
  canonical bundle digest of the content it verified. Pinned revisions and
  content digests never move during an upgrade.
- `skillctl install`, `skillctl pin`, and `skillctl rollback` write the current
  version.
- A lockfile that declares the current version may not carry `timestamps`.

Version 0 and versions above the current one are rejected.

## Restore and verification behavior

- `skillctl restore --frozen` reconstructs from an empty cache, verifies
  content against the pinned digests, and never changes the lockfile.
- A locked revision is acquired literally. The current branch HEAD is never a
  substitute for a missing revision.
- `skillctl verify` inspects state without repairing it and never contacts a
  source. It reports integrity, cache state, portability, policy eligibility,
  and freshness separately.
- Offline restore (`--offline`) succeeds only from verified cached artifacts.
  A missing artifact is reported, never worked around.

## Limits of the guarantee

A matching bundle digest proves that the supplied content is the content that
was pinned. It does not prove that anyone reviewed the content, that the
content is safe, or that a skill which fetches external scripts at runtime will
behave identically. Organizational approval is a separate concern; see
`plans/001` sections 9 and 10.
