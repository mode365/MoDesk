#!/usr/bin/env bash
# Fetch the tailcat (custom mesh tunnel) binary into res/tailcat/ for Linux
# targets. The official v0.7.0 release ships linux binaries for amd64, arm64
# and armv7; the sha256 is read from the release checksums.txt at runtime.
# Usage: ./fetch.sh [amd64|arm64|armv7]   (default: host arch mapping)
set -euo pipefail

ver="0.7.0"
arch="${1:-}"
if [ -z "$arch" ]; then
  case "$(uname -m)" in
    x86_64|amd64) arch="amd64" ;;
    aarch64|arm64) arch="arm64" ;;
    armv7l|armhf) arch="armv7" ;;
    *) echo "unsupported host arch $(uname -m), pass amd64|arm64|armv7 explicitly"; exit 1 ;;
  esac
fi
case "$arch" in
  amd64|arm64|armv7) ;;
  *) echo "unsupported arch: $arch (expected amd64|arm64|armv7)"; exit 1 ;;
esac

dir="$(cd "$(dirname "$0")" && pwd)"
out="$dir/tailcat"
if [ -f "$out" ]; then echo "tailcat already present"; exit 0; fi

asset="tailcat_${ver}_linux_${arch}.tar.gz"
url="https://github.com/tailscale/tailcat/releases/download/v${ver}/${asset}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

curl -L --fail -sS --retry 2 --connect-timeout 20 --max-time 600 -o "$tmp/$asset" "$url"

# Verify against the official checksums.txt shipped with the release.
sums="$tmp/checksums.txt"
if curl -L --fail -sS --retry 1 --connect-timeout 20 --max-time 120 \
     -o "$sums" "https://github.com/tailscale/tailcat/releases/download/v${ver}/checksums.txt"; then
  expected="$(grep " ${asset}\$" "$sums" | awk '{print $1}')"
  actual="$(sha256sum "$tmp/$asset" | awk '{print $1}')"
  if [ -n "$expected" ] && [ "$actual" != "$expected" ]; then
    echo "sha256 mismatch: $actual (expected $expected)"; exit 1
  fi
  echo "sha256 ok: $actual"
else
  echo "warning: checksums.txt unavailable, skipping sha256 verification"
fi

tar -xzf "$tmp/$asset" -C "$tmp"
bin="$(find "$tmp" -type f -name tailcat | head -n1)"
if [ -z "$bin" ]; then echo "tailcat binary not found in archive"; exit 1; fi
cp "$bin" "$out"
chmod a+x "$out"
echo "tailcat ${ver} linux_${arch} installed at $out"
