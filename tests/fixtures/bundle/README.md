# Canonical bundle fixtures

`canonical/` is the deterministic fixture for the canonical bundle format
described in `docs/bundle-format.md`.

- The tree is fixed. Any change to its paths, bytes, or the executable bit on
  `scripts/run.sh` changes the digest asserted by
  `src/bundle.rs::tests::committed_fixture_digest_is_pinned`.
- That assertion is the compatibility boundary for the hash format. If a format
  change moves the digest, bump `CANONICAL_BUNDLE_FORMAT_VERSION` and document
  the migration instead of editing the fixture to keep the old value.

Collision cases are not committed as directories. A checkout on a
case-insensitive filesystem cannot represent two paths that differ only by
case, so those cases are constructed in temporary directories by the tests in
`src/bundle.rs` (`case_and_unicode_folds_are_rejected_as_collisions`).
