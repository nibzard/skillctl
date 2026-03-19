#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/package-release.sh --binary <path> --version <tag> --target <rust-target> --output <dir>

Create a deterministic release archive for one built skillctl binary.
EOF
}

binary_path=""
version=""
target=""
output_dir=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --binary)
      binary_path="${2:-}"
      shift 2
      ;;
    --version)
      version="${2:-}"
      shift 2
      ;;
    --target)
      target="${2:-}"
      shift 2
      ;;
    --output)
      output_dir="${2:-}"
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

if [[ -z "$binary_path" || -z "$version" || -z "$target" || -z "$output_dir" ]]; then
  printf 'error: --binary, --version, --target, and --output are required\n' >&2
  usage >&2
  exit 1
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mkdir -p "$output_dir"

python_bin="${PYTHON:-}"
if [[ -z "$python_bin" ]]; then
  if command -v python3 >/dev/null 2>&1; then
    python_bin="python3"
  elif command -v python >/dev/null 2>&1; then
    python_bin="python"
  else
    printf 'error: python3 or python is required to package release archives\n' >&2
    exit 1
  fi
fi

"$python_bin" "$repo_root/scripts/package-release.py" \
  --repo-root "$repo_root" \
  --binary "$binary_path" \
  --version "$version" \
  --target "$target" \
  --output "$output_dir"
