//! Human-readable detail lines for the inventory commands.
//!
//! `list`, `path`, and `history` answer the question "what exists". Their
//! default human output therefore lists the rows it found instead of stopping
//! at a count. Each renderer reads the same payload the JSON contract emits, so
//! the human view and `--json` can never disagree about the facts.

use serde_json::Value;

/// Render the detail lines for `skillctl list` from its response payload.
pub fn list_detail_lines(data: &Value) -> Vec<String> {
    let Some(skills) = data.get("skills").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    for skill in skills {
        let name = string_field(skill, "skill").unwrap_or_else(|| "(unnamed)".to_string());
        let scope = string_field(skill, "scope").unwrap_or_else(|| "unknown".to_string());
        let source = skill
            .get("source")
            .and_then(|source| string_field(source, "type"))
            .unwrap_or_else(|| "unknown".to_string());
        let revision = string_field(skill, "resolved_revision");

        let mut line = format!("  {name} [{scope}] {source}");
        if let Some(revision) = revision {
            line.push_str(&format!(" {revision}"));
        }
        lines.push(line);

        if let Some(url) = source_url(skill.get("source")) {
            lines.push(format!("    source: {url}"));
        }

        let projections = projection_targets(skill);
        lines.push(format!(
            "    projections: {}",
            if projections.is_empty() {
                "none".to_string()
            } else {
                projections.join(", ")
            }
        ));

        if let Some(state) = list_state_line(skill) {
            lines.push(state);
        }
    }
    lines
}

/// Render the detail lines for `skillctl path` from its response payload.
pub fn path_detail_lines(data: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(scope) = string_field(data, "scope") {
        lines.push(format!("  scope: {scope}"));
    }
    if let Some(source) = source_line(data.get("source")) {
        lines.push(format!("  {source}"));
    }
    if let Some(revision) = string_field(data, "resolved_revision") {
        lines.push(format!("  revision: {revision}"));
    }

    for (label, key) in [
        ("canonical", "canonical_root"),
        ("stored source", "stored_source_root"),
        ("active source", "active_source_root"),
        ("overlay", "overlay_root"),
    ] {
        lines.push(format!(
            "  {label}: {}",
            string_field(data, key).unwrap_or_else(|| "none".to_string())
        ));
    }

    if let Some(roots) = data.get("planned_roots").and_then(Value::as_array) {
        for root in roots {
            let target = string_field(root, "target").unwrap_or_else(|| "?".to_string());
            let path = string_field(root, "path").unwrap_or_else(|| "none".to_string());
            lines.push(format!("  planned root ({target}): {path}"));
        }
    }
    if let Some(projections) = data.get("projections").and_then(Value::as_array) {
        for projection in projections {
            let target = string_field(projection, "target").unwrap_or_else(|| "?".to_string());
            let path = string_field(projection, "path").unwrap_or_else(|| "none".to_string());
            lines.push(format!("  projection ({target}): {path}"));
        }
    }
    lines
}

/// Render the detail lines for `skillctl history` from its response payload.
pub fn history_detail_lines(data: &Value) -> Vec<String> {
    let Some(entries) = data.get("entries").and_then(Value::as_array) else {
        return Vec::new();
    };

    entries
        .iter()
        .map(|entry| {
            let occurred_at =
                string_field(entry, "occurred_at").unwrap_or_else(|| "unknown".to_string());
            let kind = string_field(entry, "kind").unwrap_or_else(|| "unknown".to_string());
            let skill = string_field(entry, "skill_id").unwrap_or_else(|| "-".to_string());
            let revision = history_revision(entry).unwrap_or_else(|| "-".to_string());
            let summary = string_field(entry, "summary").unwrap_or_default();
            format!("  {occurred_at}  {kind}  {skill}  {revision}  {summary}")
        })
        .collect()
}

/// The revision clause of one history event, when the event carries one.
fn history_revision(entry: &Value) -> Option<String> {
    let details = entry.get("details")?;
    match string_field(entry, "kind").as_deref() {
        Some("install" | "detach" | "fork") => string_field(details, "resolved_revision"),
        Some("update-applied") => {
            let resolved = string_field(details, "resolved_revision")?;
            match string_field(details, "previous_revision") {
                Some(previous) if previous != resolved => Some(format!("{previous} -> {resolved}")),
                _ => Some(resolved),
            }
        }
        Some("pin") => match (
            string_field(details, "requested_reference"),
            string_field(details, "resolved_revision"),
        ) {
            (Some(requested), Some(resolved)) if requested != resolved => {
                Some(format!("{requested} -> {resolved}"))
            }
            (_, Some(resolved)) => Some(resolved),
            (Some(requested), None) => Some(requested),
            (None, None) => None,
        },
        Some("rollback") => match (
            string_field(details, "from_reference"),
            string_field(details, "to_reference"),
        ) {
            (Some(from), Some(to)) => Some(format!("{from} -> {to}")),
            (Some(from), None) => Some(from),
            (None, Some(to)) => Some(to),
            (None, None) => None,
        },
        Some("update-check") => {
            let pinned = string_field(details, "pinned_revision")?;
            match string_field(details, "latest_revision") {
                Some(latest) if latest != pinned => Some(format!("{pinned} -> {latest}")),
                _ => Some(pinned),
            }
        }
        _ => None,
    }
}

/// The source line of `skillctl path`: type, URL, and subpath.
///
/// A canonical local skill carries no source URL, so its line names the class
/// alone.
fn source_line(source: Option<&Value>) -> Option<String> {
    let source = source?;
    let kind = string_field(source, "type").unwrap_or_else(|| "unknown".to_string());
    match source_url(Some(source)) {
        Some(url) => Some(format!("source: {kind} {url}")),
        None => Some(format!("source: {kind}")),
    }
}

/// The source URL clause of one listed skill, with its subpath.
///
/// The bundled skill names the skill in its own URL, so its subpath would only
/// repeat what the URL already says.
fn source_url(source: Option<&Value>) -> Option<String> {
    let url = source.and_then(|source| string_field(source, "url"))?;
    let bundled = source
        .and_then(|source| string_field(source, "type"))
        .is_some_and(|kind| kind == "built-in");
    let subpath = if bundled {
        String::new()
    } else {
        source
            .and_then(|source| string_field(source, "subpath"))
            .map_or_else(String::new, |subpath| format!(" ({subpath})"))
    };
    Some(format!("{url}{subpath}"))
}

/// The state line of one listed skill, when the skill has notable state.
fn list_state_line(skill: &Value) -> Option<String> {
    let mut state = Vec::new();
    if bool_field(skill, "detached") {
        state.push("detached".to_string());
    }
    if bool_field(skill, "forked") {
        state.push("forked".to_string());
    }
    if skill.get("overlay_root").is_some_and(Value::is_string) {
        state.push("overlay".to_string());
    }
    if skill.get("managed_import_enabled") == Some(&Value::Bool(false)) {
        state.push("disabled".to_string());
    }
    if let Some(count) = count_field(skill, "local_modification_count").filter(|count| *count > 0) {
        state.push(format!("{count} local modification{}", plural(count)));
    }
    if let Some(count) = count_field(skill, "rollback_count").filter(|count| *count > 0) {
        state.push(format!("{count} rollback{}", plural(count)));
    }

    if state.is_empty() {
        None
    } else {
        Some(format!("    state: {}", state.join(", ")))
    }
}

/// The projected runtime targets of one listed skill, in payload order.
fn projection_targets(skill: &Value) -> Vec<String> {
    skill
        .get("projections")
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |projections| {
            projections
                .iter()
                .filter_map(|projection| string_field(projection, "target"))
                .collect()
        })
}

/// Read one string field from a JSON object.
fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Read one boolean field from a JSON object.
fn bool_field(value: &Value, key: &str) -> bool {
    value.get(key) == Some(&Value::Bool(true))
}

/// Read one count field from a JSON object.
fn count_field(value: &Value, key: &str) -> Option<usize> {
    match value.get(key) {
        Some(Value::Number(number)) => number.as_u64().map(|count| count as usize),
        _ => None,
    }
}

/// The plural suffix of one count.
fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn list_lines_cover_each_skill_and_its_state() {
        let data = json!({
            "skills": [
                {
                    "skill": "demo-skill",
                    "scope": "workspace",
                    "source": {"type": "local-path", "url": "file:///repo", "subpath": "skills/demo-skill"},
                    "resolved_revision": "sha256:aa",
                    "detached": false,
                    "forked": false,
                    "managed_import_enabled": true,
                    "overlay_root": null,
                    "local_modification_count": 2,
                    "rollback_count": 1,
                    "projections": [
                        {"target": "codex"},
                        {"target": "opencode"}
                    ],
                },
                {
                    "skill": "alpha",
                    "scope": "user",
                    "source": {"type": "built-in", "url": "builtin://skillctl", "subpath": "skillctl"},
                    "resolved_revision": "0.1.0",
                    "managed_import_enabled": null,
                    "projections": [],
                },
            ]
        });

        assert_eq!(
            list_detail_lines(&data),
            vec![
                "  demo-skill [workspace] local-path sha256:aa".to_string(),
                "    source: file:///repo (skills/demo-skill)".to_string(),
                "    projections: codex, opencode".to_string(),
                "    state: 2 local modifications, 1 rollback".to_string(),
                "  alpha [user] built-in 0.1.0".to_string(),
                "    source: builtin://skillctl".to_string(),
                "    projections: none".to_string(),
            ]
        );
    }

    #[test]
    fn path_lines_list_each_concrete_path() {
        let data = json!({
            "scope": "workspace",
            "source": {"type": "git", "url": "https://example.com/skills.git", "subpath": "skills/alpha"},
            "resolved_revision": "0123456789abcdef",
            "canonical_root": null,
            "stored_source_root": "/store/imports/workspace/key/alpha",
            "active_source_root": "/store/imports/workspace/key/alpha/skills/alpha",
            "overlay_root": ".agents/overlays/alpha",
            "planned_roots": [{"target": "codex", "path": ".codex/skills/alpha"}],
            "projections": [{"target": "codex", "path": ".codex/skills/alpha"}],
        });

        assert_eq!(
            path_detail_lines(&data),
            vec![
                "  scope: workspace".to_string(),
                "  source: git https://example.com/skills.git (skills/alpha)".to_string(),
                "  revision: 0123456789abcdef".to_string(),
                "  canonical: none".to_string(),
                "  stored source: /store/imports/workspace/key/alpha".to_string(),
                "  active source: /store/imports/workspace/key/alpha/skills/alpha".to_string(),
                "  overlay: .agents/overlays/alpha".to_string(),
                "  planned root (codex): .codex/skills/alpha".to_string(),
                "  projection (codex): .codex/skills/alpha".to_string(),
            ]
        );
    }

    #[test]
    fn history_lines_carry_kind_revision_and_time() {
        let data = json!({
            "entries": [
                {
                    "occurred_at": "2026-03-19T11:00:00Z",
                    "kind": "install",
                    "skill_id": "alpha",
                    "summary": "Installed alpha at 0123456789abcdef",
                    "details": {"resolved_revision": "0123456789abcdef"},
                },
                {
                    "occurred_at": "2026-03-19T11:01:30Z",
                    "kind": "pin",
                    "skill_id": "alpha",
                    "summary": "Pinned alpha to main",
                    "details": {"requested_reference": "main", "resolved_revision": "0123456789abcdef"},
                },
                {
                    "occurred_at": "2026-03-19T11:01:45Z",
                    "kind": "update-check",
                    "skill_id": "alpha",
                    "summary": "Checked alpha for updates",
                    "details": {"pinned_revision": "0123456789abcdef", "latest_revision": "2222"},
                },
                {
                    "occurred_at": "2026-03-19T11:02:00Z",
                    "kind": "rollback",
                    "skill_id": "alpha",
                    "summary": "Rolled back alpha",
                    "details": {"from_reference": "1111", "to_reference": "0123456789abcdef"},
                },
                {
                    "occurred_at": "2026-03-19T11:03:00Z",
                    "kind": "projection",
                    "skill_id": "alpha",
                    "summary": "Projected alpha into codex",
                    "details": {"target": "codex"},
                },
            ]
        });

        assert_eq!(
            history_detail_lines(&data),
            vec![
                "  2026-03-19T11:00:00Z  install  alpha  0123456789abcdef  Installed alpha at 0123456789abcdef".to_string(),
                "  2026-03-19T11:01:30Z  pin  alpha  main -> 0123456789abcdef  Pinned alpha to main".to_string(),
                "  2026-03-19T11:01:45Z  update-check  alpha  0123456789abcdef -> 2222  Checked alpha for updates".to_string(),
                "  2026-03-19T11:02:00Z  rollback  alpha  1111 -> 0123456789abcdef  Rolled back alpha".to_string(),
                "  2026-03-19T11:03:00Z  projection  alpha  -  Projected alpha into codex".to_string(),
            ]
        );
    }
}
