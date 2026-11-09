#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: release-smoke-test.sh --binary <path> [--home <dir>] [--workdir <dir>]

Verify that a built skillctl binary can complete a sane first run in isolation.

--home routes HOME into the binary, so the first-run bootstrap writes only
inside that directory.
EOF
}

require_flag_value() {
  local flag="$1"
  if [[ $# -lt 2 || -z "${2:-}" ]]; then
    printf 'error: %s requires a value\n' "$flag" >&2
    usage >&2
    exit 1
  fi
}

binary_path=""
home_dir=""
work_dir=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --binary)
      require_flag_value "$@"
      binary_path="$2"
      shift 2
      ;;
    --home)
      require_flag_value "$@"
      home_dir="$2"
      shift 2
      ;;
    --workdir)
      require_flag_value "$@"
      work_dir="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf 'error: unknown argument %s\n' "$1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

if [[ -z "$binary_path" ]]; then
  printf 'error: --binary is required\n' >&2
  usage >&2
  exit 1
fi

if [[ -z "$home_dir" ]]; then
  home_dir="$(mktemp -d)"
fi

if [[ -z "$work_dir" ]]; then
  work_dir="$(mktemp -d)"
fi

mkdir -p "$home_dir" "$work_dir"

# The binary resolves its user root from HOME, so the smoke test exports the
# isolated home itself. Without this, a first run bootstraps the bundled skill
# and the state store into the caller's real HOME and the assertion below fails
# against a directory nothing wrote to.
status_json="$work_dir/telemetry-status.json"
HOME="$home_dir" "$binary_path" --json telemetry status >"$status_json"

if ! grep -Eq '"ok"[[:space:]]*:[[:space:]]*true' "$status_json"; then
  printf 'error: telemetry status did not report success\n' >&2
  cat "$status_json" >&2 || true
  exit 1
fi

if [[ ! -f "$home_dir/.agents/skills/skillctl/SKILL.md" ]]; then
  printf 'error: bundled skill was not installed into %s\n' "$home_dir/.agents/skills/skillctl" >&2
  exit 1
fi

printf 'Smoke test passed for %s\n' "$binary_path"
