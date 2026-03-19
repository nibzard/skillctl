# Note: `nucleo-ext` fit for `skillctl`

`skillctl` is primarily a deterministic skill lifecycle manager, not a search-heavy application. Its current TUI is a read-only text dashboard, and its interactive install flow uses explicit numbered or exact-name selection rather than fuzzy matching.

Because of that, `nucleo-ext` is not a strong fit for the current product surface. It would only become useful if `skillctl` grows a real interactive TUI or picker with live fuzzy filtering, faceted views, or custom ranking based on local history or recency.

Recommendation:

- Do not add `nucleo-ext` now.
- Revisit it later if `skillctl` adds interactive fuzzy search.
- If fuzzy matching is needed later, compare upstream `nucleo` with `nucleo-ext`; choose `nucleo-ext` only if callback-based filtering and custom scoring are actually required.

Reasons:

- Current UX does not need fuzzy search.
- `skillctl` emphasizes deterministic CLI/JSON/MCP behavior.
- Adding a new matcher now would increase dependency and maintenance cost without clear product value.
- `nucleo-ext` is `MPL-2.0`, while `skillctl` is `MIT`, so adoption should be a deliberate licensing decision.

References:

- Local project:
  - `README.md`
  - `spec.md`
  - `src/tui.rs`
  - `src/source.rs`
  - `src/cli.rs`
  - `Cargo.toml`
- External:
  - https://github.com/atuinsh/nucleo-ext
  - https://raw.githubusercontent.com/atuinsh/nucleo-ext/main/README.md
  - https://raw.githubusercontent.com/atuinsh/nucleo-ext/main/Cargo.toml
