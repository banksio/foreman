#!/usr/bin/env bash
# Keep all execution in one compound command so a truncated pipe cannot install.
{
  set -euo pipefail

  die() {
    printf 'foreman installer: %s\n' "$*" >&2
    exit 1
  }

  tag=v0.2.0
  standalone=false
  while [[ $# != 0 ]]; do
    case "$1" in
      --version)
        [[ $# -ge 2 ]] || die 'Use --version vMAJOR.MINOR.PATCH.'
        tag=$2
        shift
        ;;
      --standalone) standalone=true ;;
      -h|--help)
        printf 'Install Foreman on a supported Debian-family host.\n\nUsage: bash install.sh [--version vMAJOR.MINOR.PATCH] [--standalone]\n'
        exit 0
        ;;
      *) die "Unknown argument: $1" ;;
    esac
    shift
  done
  [[ $tag =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] || die 'Expected a stable tag such as v0.2.0.'

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
  temp=$(mktemp -d "${TMPDIR:-/tmp}/foreman-install.XXXXXXXX")
  trap 'rm -rf -- "$temp"' EXIT
  release=https://github.com/banksio/foreman/releases/download/$tag
  asset=foreman_${version}_$arch.deb

  fetch() {
    curl --disable --proto '=https' --proto-redir '=https' --tlsv1.2 --fail --silent --show-error \
      --location --retry 3 --connect-timeout 15 --max-time 180 --max-filesize "${2:-67108864}" \
      --output "$temp/$1" "$release/$1"
  }

  if $standalone; then
    for cmd in jq openssl; do
      command -v "$cmd" >/dev/null || die "Standalone installation requires $cmd."
    done
    [[ ! -e /usr/bin/foreman ]] || die 'Remove the Debian Foreman package before switching to standalone.'
    cat > "$temp/root.pem" <<'FOREMAN_ROOT_KEY'
-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAIE0/ph5qM2blfwh6w9SVjLmZPnbBU3TEElGXhyDeWD0=
-----END PUBLIC KEY-----
FOREMAN_ROOT_KEY
    fetch keyring.json 65536
    fetch keyring.sig 64
    fetch manifest.json 65536
    fetch manifest.sig 64
    verify() {
      openssl pkeyutl -verify -rawin -pubin -inkey "$temp/$1" -in "$temp/$2" -sigfile "$temp/$3" >/dev/null \
        || die "Signature verification failed for $2."
    }
    verify root.pem keyring.json keyring.sig
    # Extract one current release key from the root-authorized keyring.
    jq -er --slurpfile manifest "$temp/manifest.json" '
      select(.schema_version == 1 and .revision == $manifest[0].keyring_revision) |
      [.keys[] | select(.id == $manifest[0].key_id and (.expires_at | fromdateiso8601) > now
                       and (.expires_at | fromdateiso8601) >= ($manifest[0].expires_at | fromdateiso8601))] |
      select(length == 1) | .[0].public_key
    ' "$temp/keyring.json" > "$temp/signer.pem" || die 'No valid Foreman release signing key.'
    verify signer.pem manifest.json manifest.sig

    asset=foreman-linux-$arch
    checksum=$(jq -er --arg version "$version" --arg platform "linux-$arch" --arg file "$asset" '
      select(.schema_version == 1 and .version == $version and (.expires_at | fromdateiso8601) > now) |
      [.artifacts[] | select(.platform == $platform)] | select(length == 1 and .[0].file == $file) |
      .[0].sha256 | select(test("^[0-9a-f]{64}$"))
    ' "$temp/manifest.json") || die 'Invalid standalone release manifest.'
    fetch "$asset"
    (
      cd "$temp"
      printf '%s  %s\n' "$checksum" "$asset" | sha256sum --check --status
    ) || die "Signed checksum mismatch for $asset."
    chmod 0700 "$temp/$asset"
    "${elevate[@]}" "$temp/$asset" __install-standalone "$temp"
    /usr/local/bin/foreman --version
    exit 0
  fi

  [[ ! -e /usr/local/lib/foreman/receipt.json ]] || die 'This host uses standalone Foreman. Use sudo foreman self update.'
  installed=$(dpkg-query -W -f='${Version}' foreman 2>/dev/null || true)
  if [[ -n $installed ]] && dpkg --compare-versions "$installed" gt "$version"; then
    die "Installed Foreman $installed is newer than $version."
  fi

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
