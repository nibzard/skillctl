# Note: minimal TUI stack for `skillctl`

For `skillctl`, a strong minimal TUI stack is:

- `ratatui` for rendering and layout
- `crossterm` for input handling and terminal control
- no async runtime
- no extra app framework
- no fuzzy matcher initially

This fits the current architecture well: keep the domain logic in the existing CLI/runtime modules, build a small typed TUI state model, and let the UI render that state without owning lifecycle behavior.

Recommended approach:

- Keep `skillctl tui` read-only first.
- Add simple key navigation and lightweight filtering over already-loaded data.
- Start with plain substring matching if search is needed.
- Only add a fuzzy engine like `nucleo` later if real interactive search becomes necessary.

Why this is the right minimal stack:

- It keeps dependencies small.
- It matches the project’s deterministic CLI and JSON-first design.
- It avoids pushing business logic into the UI layer.
- It leaves room to grow into a richer terminal UI later without a rewrite.

References:

- `spec.md`
- `src/tui.rs`
- `src/runtime.rs`
