#!/usr/bin/env bash
# Fork-only (udinCraft): installs this Linux tarball for the current user, without sudo.
#
#   ./install.sh               install into ~/.local (or $PREFIX)
#   ./install.sh --uninstall   remove what an install put there
#
# Binaries go to $PREFIX/bin; the desktop file (with absolute Exec and TryExec), MIME types,
# AppStream metadata, icons and any data under share/ go to $PREFIX/share. Run it from the
# unpacked tarball. Make sure $PREFIX/bin is on your PATH for the command-line tools.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"
UNINSTALL=0
case "${1:-}" in
  "") ;;
  --uninstall) UNINSTALL=1 ;;
  -h | --help) sed -n '2,10p' "$0"; exit 0 ;;
  *) echo "unknown argument: $1" >&2; exit 2 ;;
esac

[[ -d "$HERE/bin" && -d "$HERE/share" ]] || { echo "error: run this from the unpacked tarball (bin/ and share/ missing)" >&2; exit 1; }

refresh_caches() {
  if command -v update-mime-database >/dev/null; then update-mime-database "$PREFIX/share/mime" || true; fi
  if command -v update-desktop-database >/dev/null; then update-desktop-database "$PREFIX/share/applications" || true; fi
  if command -v gtk-update-icon-cache >/dev/null && [[ -d "$PREFIX/share/icons/hicolor" ]]; then
    gtk-update-icon-cache -f -t "$PREFIX/share/icons/hicolor" || true
  fi
  return 0
}

if [[ "$UNINSTALL" = 1 ]]; then
  for f in "$HERE"/bin/*; do rm -f "$PREFIX/bin/$(basename "$f")"; done
  (cd "$HERE/share" && find . -type f) | while read -r rel; do rm -f "$PREFIX/share/${rel#./}"; done
  refresh_caches
  echo "removed from $PREFIX"
  exit 0
fi

mkdir -p "$PREFIX/bin" "$PREFIX/share"
for f in "$HERE"/bin/*; do install -m755 "$f" "$PREFIX/bin/$(basename "$f")"; done
cp -R "$HERE/share/." "$PREFIX/share/"
# The packaged desktop files name the program alone; point them at this install.
for desktop in "$HERE"/share/applications/*.desktop; do
  [[ -f "$desktop" ]] || continue
  target="$PREFIX/share/applications/$(basename "$desktop")"
  sed -E -e "s|^Exec=([^ /]+)|Exec=$PREFIX/bin/\\1|" -e "s|^TryExec=([^ /]+)|TryExec=$PREFIX/bin/\\1|" \
    "$desktop" >"$target"
  chmod 644 "$target"
done
refresh_caches

echo "installed into $PREFIX:"
for f in "$HERE"/bin/*; do echo "  $PREFIX/bin/$(basename "$f")"; done
case ":$PATH:" in
  *":$PREFIX/bin:"*) ;;
  *) echo "note: $PREFIX/bin is not on your PATH; add it to run the command-line tools by name" ;;
esac
