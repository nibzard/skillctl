#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: release-smoke-test.sh --binary <path> [--home <dir>] [--workdir <dir>]

Verify that a built skillctl binary can complete a sane first run in isolation.
EOF
}

binary_path=""
home_dir=""
work_dir=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --binary)
      binary_path="${2:-}"
      shift 2
      ;;
    --home)
      home_dir="${2:-}"
      shift 2
      ;;
    --workdir)
      work_dir="${2:-}"
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

"$binary_path" --json telemetry status >"$work_dir/telemetry-status.json"

if [[ ! -f "$home_dir/.agents/skills/skillctl/SKILL.md" ]]; then
  printf 'error: bundled skill was not installed into %s\n' "$home_dir/.agents/skills/skillctl" >&2
  exit 1
fi

printf 'Smoke test passed for %s\n' "$binary_path"
