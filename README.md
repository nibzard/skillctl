# skillctl

`skillctl` is a local-first, cross-agent skill manager for the open `SKILL.md` ecosystem.

The project is focused on two core problems:

- installing and updating skills across multiple agent runtimes,
- tracking local history and optional telemetry for managed skills.

The current product direction is defined in [spec.md](./spec.md).

At a high level, `skillctl` is intended to:

- use `.agents/` as the default workspace control plane,
- install skills from public Git repos and local sources,
- pin installed skills to exact revisions,
- preserve local changes through overlays and rollback,
- project effective copies into agent-visible roots,
- help humans and agents debug why a skill is missing, stale, or conflicting.

The implementation target is Rust, with test-driven development as a core engineering requirement.
