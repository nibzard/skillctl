//! Field and serialization rules shared by the workspace documents.
//!
//! The manifest and the lockfile record the same identities, such as import
//! ids, source URLs, and subpaths, so both must enforce one rule set. This
//! module owns that set and returns plain messages; each document wraps a
//! message in its own error variant.

/// Directory names Windows reserves for devices.
const WINDOWS_RESERVED_DIR_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Whether `name` is reserved as a device name on Windows.
///
/// Windows resolves these names, with or without an extension, to devices
/// instead of files, so a directory of that name cannot be created there.
/// Both documents check the name on every platform, so an unportable
/// declaration fails when it is written, not when a Windows install tries to
/// create the directory.
pub(crate) fn is_windows_device_name(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or_default();
    WINDOWS_RESERVED_DIR_NAMES.contains(&base.to_ascii_lowercase().as_str())
}

/// Return the reason `value` is not a portable document identifier.
pub(crate) fn identifier_error(field: &str, value: &str) -> Option<String> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Some(format!("{field} must not be empty"));
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Some(format!(
            "{field} must start with a lowercase ASCII letter or digit: '{value}'"
        ));
    }

    if !chars
        .all(|char| char.is_ascii_lowercase() || char.is_ascii_digit() || matches!(char, '-' | '_'))
    {
        return Some(format!(
            "{field} must contain only lowercase ASCII letters, digits, '-' or '_': '{value}'"
        ));
    }

    if is_windows_device_name(value) {
        return Some(format!(
            "{field} must not use a Windows reserved device name: '{value}'"
        ));
    }

    None
}

/// Return the reason `value` is not a non-empty string without surrounding
/// or control characters.
pub(crate) fn non_empty_trimmed_error(field: &str, value: &str) -> Option<String> {
    if value.trim().is_empty() {
        return Some(format!("{field} must not be empty"));
    }
    if value.trim() != value {
        return Some(format!(
            "{field} must not contain leading or trailing whitespace"
        ));
    }
    if value.chars().any(char::is_control) {
        return Some(format!("{field} must not contain control characters"));
    }

    None
}

/// Return the reason `value` is not a single-line, non-empty token.
pub(crate) fn token_error(field: &str, value: &str) -> Option<String> {
    if let Some(message) = non_empty_trimmed_error(field, value) {
        return Some(message);
    }
    if value.chars().any(char::is_whitespace) {
        return Some(format!("{field} must not contain whitespace: '{value}'"));
    }

    None
}

/// Return the reason `value` is not a portable relative path.
pub(crate) fn relative_path_error(field: &str, value: &str) -> Option<String> {
    if let Some(message) = non_empty_trimmed_error(field, value) {
        return Some(message);
    }

    if value.starts_with('/') {
        return Some(format!(
            "{field} must be relative, found absolute path '{value}'"
        ));
    }
    if value.contains('\\') {
        return Some(format!("{field} must use '/' separators: '{value}'"));
    }
    if value.contains(':') {
        return Some(format!(
            "{field} must not contain ':' so it remains portable: '{value}'"
        ));
    }
    if value.ends_with('/') {
        return Some(format!("{field} must not end with '/': '{value}'"));
    }

    for segment in value.split('/') {
        if segment.is_empty() {
            return Some(format!(
                "{field} must not contain empty path segments: '{value}'"
            ));
        }
        if matches!(segment, "." | "..") {
            return Some(format!(
                "{field} must not contain '.' or '..' segments: '{value}'"
            ));
        }
    }

    None
}

/// Normalize serialized YAML into the stable document shape.
///
/// The formatter drops the document start marker, separates top-level keys
/// with one blank line, and indents items of a top-level sequence.
pub(crate) fn format_document_yaml(raw: &str) -> String {
    let raw = raw.strip_prefix("---\n").unwrap_or(raw);
    let mut formatted = String::new();
    let mut saw_top_level_key = false;
    let mut in_top_level_sequence = false;

    for line in raw.lines() {
        let is_top_level_key = !line.is_empty()
            && !line.starts_with(' ')
            && !line.starts_with("- ")
            && line.contains(':');
        if is_top_level_key && saw_top_level_key {
            formatted.push('\n');
        }
        if is_top_level_key {
            saw_top_level_key = true;
            in_top_level_sequence = false;
            formatted.push_str(line);
            formatted.push('\n');
            continue;
        }

        if line.starts_with("- ") {
            in_top_level_sequence = true;
            formatted.push_str("  ");
            formatted.push_str(line);
            formatted.push('\n');
            continue;
        }

        if in_top_level_sequence && line.starts_with("  ") {
            formatted.push_str("  ");
        } else if !line.starts_with(' ') {
            in_top_level_sequence = false;
        }

        formatted.push_str(line);
        formatted.push('\n');
    }

    formatted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_reject_windows_reserved_device_names() {
        for reserved in ["con", "nul", "aux", "prn", "com1", "lpt9"] {
            assert_eq!(
                identifier_error("imports[].id", reserved),
                Some(format!(
                    "imports[].id must not use a Windows reserved device name: '{reserved}'"
                )),
                "reserved name '{reserved}' must be rejected on every platform",
            );
        }

        assert_eq!(identifier_error("imports[].id", "ai-sdk"), None);
        assert_eq!(identifier_error("imports[].id", "code_review"), None);
    }

    #[test]
    fn tokens_reject_control_characters() {
        assert_eq!(
            token_error("imports[].url", "https://example.com/\u{7}skills"),
            Some("imports[].url must not contain control characters".to_string()),
        );
        assert_eq!(
            token_error("imports[].url", "https://example.com skills"),
            Some(
                "imports[].url must not contain whitespace: \
                  'https://example.com skills'"
                    .to_string()
            ),
        );
        assert_eq!(token_error("imports[].url", "https://example.com"), None);
    }

    #[test]
    fn relative_paths_reject_traversal_segments() {
        assert_eq!(
            relative_path_error("imports[].path", "../skills/ai-sdk"),
            Some(
                "imports[].path must not contain '.' or '..' segments: \
                  '../skills/ai-sdk'"
                    .to_string()
            ),
        );
        assert_eq!(relative_path_error("imports[].path", "skills/ai-sdk"), None);
    }
}
