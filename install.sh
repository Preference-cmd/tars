#!/bin/sh
set -eu

DEFAULT_BASE_URL="https://github.com/Preference-cmd/tars"

tmpdir=""
tmp_bin=""

cleanup() {
  if [ -n "$tmp_bin" ] && [ -e "$tmp_bin" ]; then
    rm -f "$tmp_bin"
  fi
  if [ -n "$tmpdir" ] && [ -d "$tmpdir" ]; then
    rm -rf "$tmpdir"
  fi
}

on_signal() {
  cleanup
  trap - EXIT INT TERM
  exit 130
}

trap cleanup EXIT
trap on_signal INT TERM

usage() {
  cat <<'EOF'
Usage: install.sh [OPTIONS]

Install or upgrade the TARS (`tars`) binary with mandatory SHA-256 verification.

Options:
  --version <vX.Y.Z>    Release tag to install (default: latest release)
  --install-dir <dir>   Target bin directory (default: $HOME/.local/bin)
  --base-url <url>      Release base URL (default: https://github.com/Preference-cmd/tars)
  -h, --help            Show this help message and exit

Environment variables:
  TARS_VERSION          Same as --version
  TARS_INSTALL_DIR      Same as --install-dir
  TARS_RELEASE_BASE_URL Same as --base-url
  TARS_INSTALL_OS       Override detected OS (uname -s)
  TARS_INSTALL_ARCH     Override detected architecture (uname -m)

Supported platforms:
  macOS   arm64 / aarch64  (aarch64-apple-darwin)
  macOS   x86_64           (x86_64-apple-darwin)
  Linux   x86_64 / amd64   (x86_64-unknown-linux-gnu)
  Linux   aarch64 / arm64  (aarch64-unknown-linux-gnu)
EOF
}

die() {
  printf 'install.sh: error: %s\n' "$*" >&2
  exit 1
}

die_usage() {
  printf 'install.sh: error: %s\n' "$*" >&2
  printf 'Run "install.sh --help" for usage.\n' >&2
  exit 2
}

validate_version() {
  case "$1" in
    v[A-Za-z0-9._-]*)
      case "${1#v}" in
        ""|*[!A-Za-z0-9._-]*)
          return 1
          ;;
      esac
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

strip_trailing_slashes() {
  val="$1"
  while :; do
    case "$val" in
      /) break ;;
      */) val="${val%/}" ;;
      *) break ;;
    esac
  done
  printf '%s' "$val"
}

require_arg() {
  if [ "$#" -lt 2 ] || [ -z "$2" ]; then
    die_usage "option $1 requires a non-empty argument"
  fi
}

version="${TARS_VERSION:-}"
if [ -n "${TARS_INSTALL_DIR:-}" ]; then
  install_dir="$TARS_INSTALL_DIR"
elif [ -n "${HOME:-}" ]; then
  install_dir="${HOME}/.local/bin"
else
  install_dir=""
fi
base_url="${TARS_RELEASE_BASE_URL:-$DEFAULT_BASE_URL}"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      require_arg "$@"
      version="$2"
      shift 2
      ;;
    --version=*)
      version="${1#--version=}"
      [ -n "$version" ] || die_usage "option --version requires a non-empty argument"
      shift
      ;;
    --install-dir)
      require_arg "$@"
      install_dir="$2"
      shift 2
      ;;
    --install-dir=*)
      install_dir="${1#--install-dir=}"
      [ -n "$install_dir" ] || die_usage "option --install-dir requires a non-empty argument"
      shift
      ;;
    --base-url)
      require_arg "$@"
      base_url="$2"
      shift 2
      ;;
    --base-url=*)
      base_url="${1#--base-url=}"
      [ -n "$base_url" ] || die_usage "option --base-url requires a non-empty argument"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --)
      shift
      if [ "$#" -gt 0 ]; then
        die_usage "unexpected positional argument: $1"
      fi
      ;;
    -*)
      die_usage "unknown option: $1"
      ;;
    *)
      die_usage "unexpected argument: $1"
      ;;
  esac
done

[ -n "$install_dir" ] || die_usage "install directory cannot be empty (set HOME or pass --install-dir)"
install_dir=$(strip_trailing_slashes "$install_dir")
[ -n "$base_url" ] || die_usage "base URL cannot be empty"
base_url=$(strip_trailing_slashes "$base_url")
case "$base_url" in
  http://*|https://*) ;;
  *) die_usage "base URL must start with http:// or https://: $base_url" ;;
esac

if [ -n "$version" ] && ! validate_version "$version"; then
  die "invalid version tag '${version}' (must start with 'v' and contain only [A-Za-z0-9._-])"
fi

os_name="${TARS_INSTALL_OS:-$(uname -s)}"
arch_name="${TARS_INSTALL_ARCH:-$(uname -m)}"

case "${os_name}:${arch_name}" in
  Darwin:arm64|Darwin:aarch64)
    target="aarch64-apple-darwin"
    ;;
  Darwin:x86_64)
    target="x86_64-apple-darwin"
    ;;
  Linux:x86_64|Linux:amd64)
    target="x86_64-unknown-linux-gnu"
    ;;
  Linux:aarch64|Linux:arm64)
    target="aarch64-unknown-linux-gnu"
    ;;
  *)
    die "unsupported platform (${os_name}/${arch_name}); supported targets are macOS (arm64, x86_64) and Linux (x86_64, aarch64). Build from source with: cargo build --workspace --locked --release"
    ;;
esac

if command -v curl >/dev/null 2>&1; then
  downloader="curl"
elif command -v wget >/dev/null 2>&1; then
  downloader="wget"
else
  die "neither curl nor wget was found; please install curl or wget and retry"
fi

if command -v sha256sum >/dev/null 2>&1; then
  sha_cmd="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
  sha_cmd="shasum"
else
  die "neither sha256sum nor shasum was found; cannot verify archive checksum"
fi

command -v tar >/dev/null 2>&1 || die "tar is required to extract the release archive"

download_file() {
  url="$1"
  dest="$2"
  if [ "$downloader" = "curl" ]; then
    curl -fsSL -o "$dest" "$url" || return 1
  else
    wget -qO "$dest" "$url" || return 1
  fi
}

resolve_latest_version() {
  latest_url="${base_url}/releases/latest"
  if [ "$downloader" = "curl" ]; then
    effective_url=$(curl -fsSL -I -o /dev/null -w '%{url_effective}' "$latest_url") || return 1
  else
    headers=$(wget -q -S -O /dev/null "$latest_url" 2>&1) || return 1
    cr=$(printf '\r')
    effective_url=$(printf '%s\n' "$headers" | sed -n 's/^[[:space:]]*[Ll]ocation:[[:space:]]*//p' | tail -n 1)
    effective_url=${effective_url%"$cr"}
  fi

  case "$effective_url" in
    */releases/tag/*)
      resolved="${effective_url##*/releases/tag/}"
      resolved="${resolved%%/*}"
      resolved="${resolved%%\?*}"
      resolved="${resolved%%#*}"
      ;;
    *)
      return 1
      ;;
  esac

  validate_version "$resolved" || return 1
  printf '%s' "$resolved"
}

if [ -z "$version" ]; then
  version=$(resolve_latest_version) || die "failed to resolve latest release tag from ${base_url}/releases/latest"
fi

stem="tars-${version}-${target}"
archive="${stem}.tar.gz"
archive_url="${base_url}/releases/download/${version}/${archive}"
sums_url="${base_url}/releases/download/${version}/SHA256SUMS.txt"

tmpdir=$(mktemp -d "${TMPDIR:-/tmp}/tars-install.XXXXXX")
archive_path="${tmpdir}/${archive}"
sums_path="${tmpdir}/SHA256SUMS.txt"
tar_list_path="${tmpdir}/tar-list.txt"
extract_dir="${tmpdir}/extract"

printf 'Downloading %s (%s)...\n' "$archive" "$version"
download_file "$archive_url" "$archive_path" || die "failed to download ${archive_url}"
download_file "$sums_url" "$sums_path" || die "failed to download ${sums_url}"

cr=$(printf '\r')
tab=$(printf '\t')
ws=" ${tab}"
expected_sum=""
match_count=0

while IFS= read -r line || [ -n "$line" ]; do
  line=${line%"$cr"}
  case "$line" in
    "") continue ;;
  esac

  hash_part=${line%%[${ws}]*}
  rest_part=${line#"$hash_part"}
  trimmed_rest="$rest_part"
  while :; do
    case "$trimmed_rest" in
      " "*|"$tab"*) trimmed_rest="${trimmed_rest#?}" ;;
      *) break ;;
    esac
  done

  if [ "$rest_part" = "$trimmed_rest" ]; then
    continue
  fi

  if [ "$trimmed_rest" = "$archive" ]; then
    case "$hash_part" in
      ""|*[!0-9A-Fa-f]*)
        die "malformed SHA-256 entry for ${archive} in SHA256SUMS.txt"
        ;;
    esac
    if [ "${#hash_part}" -ne 64 ]; then
      die "malformed SHA-256 digest length for ${archive} in SHA256SUMS.txt"
    fi
    expected_sum="$hash_part"
    match_count=$((match_count + 1))
  fi
done < "$sums_path"

if [ "$match_count" -eq 0 ]; then
  die "SHA256SUMS.txt does not contain an entry for ${archive}"
elif [ "$match_count" -gt 1 ]; then
  die "SHA256SUMS.txt contains multiple entries for ${archive}"
fi

if [ "$sha_cmd" = "sha256sum" ]; then
  set -- $(sha256sum "$archive_path")
  actual_sum="$1"
else
  set -- $(shasum -a 256 "$archive_path")
  actual_sum="$1"
fi

expected_lower=$(printf '%s' "$expected_sum" | tr '[:upper:]' '[:lower:]')
actual_lower=$(printf '%s' "$actual_sum" | tr '[:upper:]' '[:lower:]')

if [ "${#actual_lower}" -ne 64 ] || [ "$actual_lower" != "$expected_lower" ]; then
  die "SHA-256 checksum mismatch for ${archive}: expected ${expected_lower}, got ${actual_lower}"
fi

tar -tzf "$archive_path" > "$tar_list_path" || die "failed to inspect archive ${archive}"
[ -s "$tar_list_path" ] || die "archive ${archive} is empty"

while IFS= read -r entry || [ -n "$entry" ]; do
  entry=${entry%"$cr"}
  case "$entry" in
    "") continue ;;
    /*)
      die "unsafe absolute path in archive: ${entry}"
      ;;
    ..|../*|*/..|*/../*)
      die "unsafe path traversal in archive: ${entry}"
      ;;
  esac
done < "$tar_list_path"

mkdir -p "$extract_dir"
tar -xzf "$archive_path" -C "$extract_dir" "${stem}/tars" || die "failed to extract ${stem}/tars from ${archive}"

extracted_stem="${extract_dir}/${stem}"
extracted_bin="${extracted_stem}/tars"

[ ! -L "$extracted_stem" ] || die "archive stem ${stem} must not be a symlink"
[ ! -L "$extracted_bin" ] || die "extracted binary ${stem}/tars must not be a symlink"
[ -f "$extracted_bin" ] || die "extracted binary ${stem}/tars is missing or not a regular file"
[ -s "$extracted_bin" ] || die "extracted binary ${stem}/tars is empty"

mkdir -p "$install_dir"
tmp_bin=$(mktemp "${install_dir}/.tars.install.XXXXXX")
cp "$extracted_bin" "$tmp_bin"
chmod 0755 "$tmp_bin"

if ! "$tmp_bin" --version >/dev/null 2>&1; then
  die "downloaded binary failed verification check (--version)"
fi

target_bin="${install_dir}/tars"
mv -f "$tmp_bin" "$target_bin"
tmp_bin=""

installed_version=$("$target_bin" --version) || die "installed binary failed to report --version"
printf 'Installed %s (%s)\n' "$target_bin" "$installed_version"

clean_install_dir=$(strip_trailing_slashes "$install_dir")
case ":${PATH:-}:" in
  *":${clean_install_dir}:"*|*":${install_dir}:"*)
    ;;
  *)
    printf 'Add %s to your PATH to run `tars` directly:\n' "$clean_install_dir"
    printf '  export PATH="%s:$PATH"\n' "$clean_install_dir"
    ;;
esac
