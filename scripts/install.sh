#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: install.sh [--version <tag>] [--install-dir <dir>]

Download and install the latest skillctl release binary for the current platform.

Environment overrides:
  SKILLCTL_VERSION           Release tag to install, for example v0.1.0
  SKILLCTL_INSTALL_DIR       Directory where the skillctl binary should be installed
  SKILLCTL_RELEASE_BASE_URL  Release base URL, defaults to GitHub releases
  SKILLCTL_REPOSITORY        GitHub repository in owner/name form
EOF
}

repository="${SKILLCTL_REPOSITORY:-nibzard/skillctl}"
default_release_base_url="https://github.com/${repository}/releases"
release_base_url="${SKILLCTL_RELEASE_BASE_URL:-$default_release_base_url}"
install_dir="${SKILLCTL_INSTALL_DIR:-${HOME}/.local/bin}"
version="${SKILLCTL_VERSION:-}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)
      version="${2:-}"
      shift 2
      ;;
    --install-dir|--bin-dir)
      install_dir="${2:-}"
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

normalize_version() {
  local raw_version="$1"
  if [[ "$raw_version" == v* ]]; then
    printf '%s\n' "$raw_version"
  else
    printf 'v%s\n' "$raw_version"
  fi
}

download() {
  local url="$1"
  local destination="$2"
  curl -fsSL "$url" -o "$destination"
}

unsupported_platform() {
  local os_name="$1"
  local arch_name="$2"
  printf '%s\n' \
    "error: unsupported platform ${os_name} ${arch_name} for published release artifacts; supported targets are Linux x86_64, macOS x86_64, macOS arm64, and Windows x86_64 (zip only)" >&2
  exit 1
}

resolve_latest_version() {
  local api_url="https://api.github.com/repos/${repository}/releases/latest"
  local response
  response="$(curl -fsSL "$api_url" | tr -d '\n')"
  local latest
  latest="$(printf '%s' "$response" | sed -n 's/.*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p')"
  if [[ -z "$latest" ]]; then
    printf 'error: failed to resolve the latest release tag from %s\n' "$api_url" >&2
    exit 1
  fi
  printf '%s\n' "$latest"
}

resolve_target() {
  local os_name
  local arch_name
  os_name="$(uname -s)"
  arch_name="$(uname -m)"

  case "$os_name" in
    Linux)
      case "$arch_name" in
        x86_64|amd64) printf 'x86_64-unknown-linux-gnu\n' ;;
        aarch64|arm64) unsupported_platform "Linux" "arm64" ;;
        *) unsupported_platform "Linux" "$arch_name" ;;
      esac
      ;;
    Darwin)
      case "$arch_name" in
        x86_64) printf 'x86_64-apple-darwin\n' ;;
        arm64|aarch64) printf 'aarch64-apple-darwin\n' ;;
        *) unsupported_platform "macOS" "$arch_name" ;;
      esac
      ;;
    MINGW*|MSYS*|CYGWIN*)
      printf 'error: use the published Windows zip artifact instead of the curl installer\n' >&2
      exit 1
      ;;
    *) unsupported_platform "$os_name" "$arch_name" ;;
  esac
}

sha256_file() {
  local path="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$path" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$path" | awk '{print $1}'
  elif command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 "$path" | awk '{print $NF}'
  else
    printf 'error: no SHA-256 tool is available (need sha256sum, shasum, or openssl)\n' >&2
    exit 1
  fi
}

if [[ -z "$version" ]]; then
  if [[ "$release_base_url" != "$default_release_base_url" ]]; then
    printf 'error: --version or SKILLCTL_VERSION is required when SKILLCTL_RELEASE_BASE_URL is overridden\n' >&2
    exit 1
  fi
  version="$(resolve_latest_version)"
else
  version="$(normalize_version "$version")"
fi

target="$(resolve_target)"
archive_name="skillctl-${version}-${target}.tar.gz"
checksums_name="skillctl-${version}-checksums.txt"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

archive_path="$tmp_dir/$archive_name"
checksums_path="$tmp_dir/$checksums_name"

download "${release_base_url}/download/${version}/${archive_name}" "$archive_path"
download "${release_base_url}/download/${version}/${checksums_name}" "$checksums_path"

expected_checksum="$(awk -v archive="$archive_name" '$2 == archive { print $1 }' "$checksums_path")"
if [[ -z "$expected_checksum" ]]; then
  printf 'error: checksum entry for %s was not found in %s\n' "$archive_name" "$checksums_name" >&2
  exit 1
fi

actual_checksum="$(sha256_file "$archive_path")"
if [[ "$expected_checksum" != "$actual_checksum" ]]; then
  printf 'error: checksum verification failed for %s\n' "$archive_name" >&2
  exit 1
fi

tar -xzf "$archive_path" -C "$tmp_dir"

mkdir -p "$install_dir"
cp "$tmp_dir/skillctl-${version}-${target}/skillctl" "$install_dir/skillctl"
chmod 755 "$install_dir/skillctl"

printf 'Installed skillctl %s to %s/skillctl\n' "$version" "$install_dir"
if [[ ":${PATH:-}:" != *":${install_dir}:"* ]]; then
  printf 'Add %s to PATH to run skillctl globally.\n' "$install_dir"
fi
