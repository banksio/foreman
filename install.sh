#!/usr/bin/env bash
# Keep all execution in one compound command so a truncated pipe cannot install.
{
  set -euo pipefail

  die() {
    printf 'foreman installer: %s\n' "$*" >&2
    exit 1
  }

  tag=v0.1.0
  case "${1:-}" in
    '')
      [[ $# == 0 ]] || die 'Use --version vMAJOR.MINOR.PATCH.'
      ;;
    --version)
      [[ $# == 2 ]] || die 'Use --version vMAJOR.MINOR.PATCH.'
      tag=$2
      ;;
    -h|--help)
      printf 'Install Foreman on a supported Debian-family host.\n\nUsage: bash install.sh [--version vMAJOR.MINOR.PATCH]\n'
      exit 0
      ;;
    *)
      die "Unknown argument: $1"
      ;;
  esac
  [[ $tag =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] || die 'Expected a stable tag such as v0.1.0.'

  [[ $(uname -s) == Linux ]] || die 'This installer requires Linux.'
  for cmd in curl apt-get dpkg dpkg-deb dpkg-query sha256sum awk mktemp; do
    command -v "$cmd" >/dev/null || die "Required command is missing: $cmd"
  done
  [[ -f /etc/debian_version ]] || die 'This installer uses Debian packages.'
  arch=$(dpkg --print-architecture)
  case "$arch" in
    arm64|amd64) ;;
    *) die "Unsupported Debian architecture: $arch. Use a 64-bit OS." ;;
  esac
  if [[ $EUID != 0 ]]; then
    command -v sudo >/dev/null || die 'Install as root or make sudo available.'
    elevate=(sudo)
  else
    elevate=()
  fi

  version=${tag#v}
  installed=$(dpkg-query -W -f='${Version}' foreman 2>/dev/null || true)
  if [[ -n $installed ]] && dpkg --compare-versions "$installed" gt "$version"; then
    die "Installed Foreman $installed is newer than $version."
  fi

  temp=$(mktemp -d "${TMPDIR:-/tmp}/foreman-install.XXXXXXXX")
  trap 'rm -rf -- "$temp"' EXIT
  release=https://github.com/banksio/foreman/releases/download/$tag
  asset=foreman_${version}_$arch.deb

  fetch() {
    curl --proto '=https' --proto-redir '=https' --tlsv1.2 --fail --silent --show-error \
      --location --retry 3 --connect-timeout 15 --max-time 180 --output "$temp/$1" "$release/$1"
  }

  fetch SHA256SUMS
  checksum=$(awk -v name="$asset" '
    ($2 == name || $2 == "*" name) {
      if (length($1) != 64 || $1 !~ /^[a-f0-9]+$/ || ++count != 1) exit 1
      hash = $1
    }
    END { if (count != 1) exit 1; print hash }
  ' "$temp/SHA256SUMS") || die "Missing or invalid checksum for $asset."
  fetch "$asset"
  (
    cd "$temp"
    printf '%s  %s\n' "$checksum" "$asset" | sha256sum --check --status
  ) || die "Checksum mismatch for $asset."
  [[ $(dpkg-deb -f "$temp/$asset" Package) == foreman ]] || die 'Unexpected package name.'
  [[ $(dpkg-deb -f "$temp/$asset" Version) == "$version" ]] || die 'Unexpected package version.'
  [[ $(dpkg-deb -f "$temp/$asset" Architecture) == "$arch" ]] || die 'Unexpected package architecture.'

  chmod 0755 "$temp"
  "${elevate[@]}" apt-get install -y --no-install-recommends "$temp/$asset"
  /usr/bin/foreman --version
}
