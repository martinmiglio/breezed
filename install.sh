#!/usr/bin/env bash
# Install or upgrade breezed from a GitHub release.
#
# The release archive and SHA-256 checksums are downloaded into a private
# temporary directory, verified, and then handed to `breezed daemon install`.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/martinmiglio/breezed/master/install.sh | bash
#   curl -fsSL https://raw.githubusercontent.com/martinmiglio/breezed/master/install.sh \
#     | bash -s -- --version 0.3.1
#   curl -fsSL https://raw.githubusercontent.com/martinmiglio/breezed/master/install.sh \
#     | bash -s -- --dry-run
#
# The release asset `install.sh` is pinned to its release version, so this also
# works without resolving `latest`:
#   curl -fsSL https://github.com/martinmiglio/breezed/releases/download/v0.3.1/install.sh | bash
#
# Environment:
#   BREEZED_VERSION               Version without the leading `v` (default: latest)
#   BREEZED_BASE_URL              Override the release download base (mirrors/testing)
#   BREEZED_VERIFY_ATTESTATION=1  Also verify GitHub build provenance with `gh`
#
# Any other arguments are forwarded to `breezed daemon install`.

set -euo pipefail
umask 077

REPO="martinmiglio/breezed"
TARGET="x86_64-unknown-linux-gnu"
DEFAULT_VERSION=""

say() { printf 'breezed-install: %s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }

usage() {
    cat <<'EOF'
Usage: install.sh [--version VERSION] [daemon install args...]

Downloads a breezed release, verifies its SHA-256, and runs
`breezed daemon install`.

Options:
  --version VERSION   Release version without the leading v (default: latest)
  -h, --help          Show this help

Environment:
  BREEZED_VERSION               Same as --version
  BREEZED_BASE_URL              Override the release download base
  BREEZED_VERIFY_ATTESTATION=1  Verify build provenance with gh (optional)

Examples:
  curl -fsSL https://raw.githubusercontent.com/martinmiglio/breezed/master/install.sh | bash
  curl -fsSL https://raw.githubusercontent.com/martinmiglio/breezed/master/install.sh | bash -s -- --version 0.3.1
  curl -fsSL https://raw.githubusercontent.com/martinmiglio/breezed/master/install.sh | bash -s -- --dry-run
EOF
}

version="${BREEZED_VERSION:-$DEFAULT_VERSION}"
install_args=()

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            [ "$#" -ge 2 ] || die "--version requires a value"
            version="$2"
            shift 2
            ;;
        --version=*)
            version="${1#*=}"
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            install_args+=("$1")
            shift
            ;;
    esac
done

case "$version" in
    *[!0-9A-Za-z.+-]*) die "invalid version: $version" ;;
esac

for tool in curl tar sha256sum awk sed mktemp; do
    command -v "$tool" >/dev/null 2>&1 || die "required command not found: $tool"
done

[ "$(uname -s)" = "Linux" ] || die "only Linux is supported"

case "$(uname -m)" in
    x86_64|amd64) ;;
    *) die "no prebuilt release for $(uname -m); build from source instead" ;;
esac

if [ -z "$version" ]; then
    say "resolving the latest release"
    version="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
        | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"v\{0,1\}\([^"]*\)".*/\1/p' \
        | head -n 1)"
    [ -n "$version" ] || die "could not resolve the latest release; pass --version or set BREEZED_VERSION"
fi

asset="breezed-$version-$TARGET.tar.gz"
base="${BREEZED_BASE_URL:-https://github.com/$REPO/releases/download/v$version}"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/breezed-install.XXXXXXXX")"
trap 'rm -rf "$tmp"' EXIT

say "downloading breezed $version ($TARGET)"
curl -fsSL --retry 3 -o "$tmp/$asset" "$base/$asset"
curl -fsSL --retry 3 -o "$tmp/SHA256SUMS" "$base/SHA256SUMS"

expected="$(awk -v asset="$asset" '$2 == asset { print $1; exit }' "$tmp/SHA256SUMS")"
[ -n "$expected" ] || die "SHA256SUMS does not list $asset"
actual="$(sha256sum "$tmp/$asset" | awk '{ print $1 }')"
[ "$expected" = "$actual" ] || die "checksum mismatch for $asset"
say "verified SHA-256: $actual"

if [ "${BREEZED_VERIFY_ATTESTATION:-0}" = "1" ]; then
    command -v gh >/dev/null 2>&1 || die "BREEZED_VERIFY_ATTESTATION=1 requires the gh CLI"
    say "verifying build provenance"
    gh attestation verify "$tmp/$asset" --repo "$REPO"
fi

tar -xzf "$tmp/$asset" -C "$tmp"
binary="$tmp/breezed-$version-$TARGET/breezed"
[ -x "$binary" ] || die "release archive did not contain an executable"

say "installing breezed $version"
if [ "${#install_args[@]}" -gt 0 ]; then
    "$binary" daemon install "${install_args[@]}"
else
    "$binary" daemon install
fi
