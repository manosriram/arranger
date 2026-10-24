#!/bin/sh
# Installs arranger from a GitHub release into ~/.local/bin, e.g. on a machine you'll add to the app:
#   curl -fsSL https://github.com/manosriram/arranger/releases/latest/download/install.sh | sh
# ARRANGER_VERSION=v0.2.0 picks a release (machines must run the same one); ARRANGER_INSTALL_DIR the folder.
set -eu

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
  *) url="$repo/download/$ARRANGER_VERSION/arranger-$target.tar.gz" ;;
esac

dir="${ARRANGER_INSTALL_DIR:-$HOME/.local/bin}"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL "$url" | tar xz -C "$tmp"
mkdir -p "$dir"
pkg="$tmp/arranger-$target"
if [ -d "$pkg/arranger.app" ]; then
  # macOS: the app goes in ~/Applications (Finder, Spotlight, the Dock); the command runs the
  # binaries inside it, so there's one copy.
  apps="$HOME/Applications"
  mkdir -p "$apps"
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
