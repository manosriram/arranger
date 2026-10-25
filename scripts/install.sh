#!/bin/sh
# Installs arranger from a GitHub release into ~/.local/bin, e.g. on a machine you'll add to the app:
#   curl -fsSL https://github.com/manosriram/arranger/releases/latest/download/install.sh | sh
# ARRANGER_VERSION=v0.2.0 picks a release (machines must run the same one); ARRANGER_INSTALL_DIR the folder.
set -eu
umask 077

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  Linux-x86_64) target=x86_64-unknown-linux-gnu ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-gnu ;;
  *) echo "arranger: no build for $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac

repo=https://github.com/manosriram/arranger/releases
case "${ARRANGER_VERSION:-latest}" in
  latest) url="$repo/latest/download/arranger-$target.tar.gz" ;;
  *)
    case "$ARRANGER_VERSION" in
      *[!A-Za-z0-9._-]* | '') echo "arranger: invalid release version" >&2; exit 1 ;;
    esac
    url="$repo/download/$ARRANGER_VERSION/arranger-$target.tar.gz" ;;
esac

dir="${ARRANGER_INSTALL_DIR:-$HOME/.local/bin}"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' 0
trap 'exit 1' HUP INT TERM
# Download completely and verify before extraction. A failed curl must never feed a partial tar.
curl --proto '=https' --tlsv1.2 -fsSL "$url" -o "$tmp/archive.tar.gz"
curl --proto '=https' --tlsv1.2 -fsSL "$url.sha256" -o "$tmp/checksum"
expected=$(awk 'NR == 1 { print $1 }' "$tmp/checksum")
case "$expected" in *[!0-9a-fA-F]* | '') echo "arranger: invalid checksum" >&2; exit 1 ;; esac
[ "${#expected}" -eq 64 ] || { echo "arranger: invalid checksum" >&2; exit 1; }
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$tmp/archive.tar.gz" | awk '{print $1}')
else
  actual=$(shasum -a 256 "$tmp/archive.tar.gz" | awk '{print $1}')
fi
[ "$expected" = "$actual" ] || { echo "arranger: checksum mismatch; nothing installed" >&2; exit 1; }
tar xzf "$tmp/archive.tar.gz" -C "$tmp"
mkdir -p "$dir"
pkg="$tmp/arranger-$target"
if [ -d "$pkg/arranger.app" ]; then
  # macOS: the app goes in ~/Applications (Finder, Spotlight, the Dock); the command runs the
  # binaries inside it, so there's one copy.
  apps="$HOME/Applications"
  mkdir -p "$apps"
  # Validate the replacement before removing a previously working installation.
  [ -x "$pkg/arranger.app/Contents/MacOS/arranger" ] && [ -x "$pkg/arranger.app/Contents/MacOS/arranger-gui" ] || exit 1
  rm -rf "$apps/arranger.app"
  mv "$pkg/arranger.app" "$apps/"
  ln -sf "$apps/arranger.app/Contents/MacOS/arranger" "$dir/arranger"
  ln -sf "$apps/arranger.app/Contents/MacOS/arranger-gui" "$dir/arranger-gui"
  echo "arranger installed to $apps/arranger.app (and the arranger command to $dir)"
else
  install -m 755 "$pkg/arranger" "$pkg/arranger-gui" "$dir/"
  echo "arranger installed to $dir"
fi
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "add $dir to your PATH" ;;
esac
