#!/usr/bin/env bash
set -euo pipefail

if [[ $# != 4 ]]; then
  printf 'Usage: %s BINARY VERSION ARCH OUTPUT_DIR\n' "$0" >&2
  exit 2
fi

binary=$1
version=$2
arch=$3
output=$4
here=$(cd -- "$(dirname -- "$0")" && pwd)
repo=$(cd -- "$here/../.." && pwd)

[[ $version =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]
[[ $arch == arm64 || $arch == amd64 ]]
[[ $(dpkg --print-architecture) == "$arch" ]]
[[ $("$binary" --version) == "foreman $version" ]]

stage=$(mktemp -d)
trap 'rm -rf -- "$stage"' EXIT
chmod 0755 "$stage"
install -d "$stage"/{DEBIAN,etc/foreman,lib/systemd/system,usr/bin,usr/share/doc/foreman}
install -m 0755 "$binary" "$stage/usr/bin/foreman"
install -m 0644 "$repo/config.example.toml" "$stage/etc/foreman/config.example.toml"
install -m 0644 "$repo/LICENSE" "$stage/usr/share/doc/foreman/copyright"
install -m 0644 "$here/"*.service "$here/"*.timer "$stage/lib/systemd/system/"
install -m 0755 "$here/preinst" "$here/postinst" "$here/prerm" "$here/postrm" "$stage/DEBIAN/"
printf '/etc/foreman/config.example.toml\n' > "$stage/DEBIAN/conffiles"
cat > "$stage/DEBIAN/control" <<EOF
Package: foreman
Version: $version
Architecture: $arch
Maintainer: Nathan Banks <banksio@users.noreply.github.com>
Section: admin
Priority: optional
Depends: apt, libc6 (>= 2.36), init-system-helpers, netcat-openbsd, systemd
Recommends: ca-certificates, curl
Homepage: https://banksio.github.io/foreman/
Description: Local maintenance for Debian-family hosts
 Foreman applies and reports host maintenance through a command-line
 interface, with optional recurring jobs and Docker Compose management.
EOF

mkdir -p -- "$output"
dpkg-deb --root-owner-group --build "$stage" "$output/foreman_${version}_$arch.deb"
