#!/usr/bin/env bash
# Fork-only (udinCraft): installs this Linux tarball for the current user, without sudo.
#
#   ./install.sh               install into ~/.local (or $PREFIX)
#   ./install.sh --uninstall   remove what an install put there
#
# Binaries go to $PREFIX/bin; the desktop file (with absolute Exec and TryExec), MIME types,
# AppStream metadata, icons and any data under share/ go to $PREFIX/share. Run it from the
# unpacked tarball. Make sure $PREFIX/bin is on your PATH for the command-line tools. It warns,
# with the distro's install command, when the keyboard libraries the apps load are missing.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"
UNINSTALL=0
case "${1:-}" in
  "") ;;
  --uninstall) UNINSTALL=1 ;;
  -h | --help) sed -n '2,12p' "$0"; exit 0 ;;
  *) echo "unknown argument: $1" >&2; exit 2 ;;
esac

[[ -d "$HERE/bin" && -d "$HERE/share" ]] || { echo "error: run this from the unpacked tarball (bin/ and share/ missing)" >&2; exit 1; }

# The window toolkit loads the keyboard libraries at run time: libxkbcommon always, and
# libxkbcommon-x11 with libxcb-xkb on X11. They are not in this tarball. Take all of them from the
# distro, so libxkbcommon-x11 matches libxkbcommon (a mismatched pair crashes the apps).
check_libraries() {
  local missing=() lib id_like
  for lib in libxkbcommon.so.0 libxkbcommon-x11.so.0 libxcb-xkb.so.1; do
    if command -v ldconfig >/dev/null && ldconfig -p 2>/dev/null | grep -q "$lib"; then continue; fi
    if command -v /sbin/ldconfig >/dev/null && /sbin/ldconfig -p 2>/dev/null | grep -q "$lib"; then continue; fi
    if compgen -G "/usr/lib*/$lib" >/dev/null || compgen -G "/usr/lib*/*/$lib" >/dev/null || compgen -G "/lib*/*/$lib" >/dev/null; then continue; fi
    missing+=("$lib")
  done
  [[ ${#missing[@]} -eq 0 ]] && return 0
  echo "warning: missing system libraries: ${missing[*]}" >&2
  echo "  The apps need libxkbcommon, libxkbcommon-x11 and libxcb-xkb from your distro (a matching pair; do not copy them in by hand). Install them with:" >&2
  id_like="$( (. /etc/os-release 2>/dev/null && echo "${ID:-} ${ID_LIKE:-}") || true)"
  case " $id_like " in
    *" arch "* | *" cachyos "* | *" manjaro "* | *" endeavouros "*) echo "    sudo pacman -S --needed libxkbcommon libxkbcommon-x11 libxcb" >&2 ;;
    *" debian "* | *" ubuntu "*) echo "    sudo apt install libxkbcommon0 libxkbcommon-x11-0 libxcb-xkb1" >&2 ;;
    *" fedora "* | *" rhel "*) echo "    sudo dnf install libxkbcommon libxkbcommon-x11 libxcb" >&2 ;;
    *)
      echo "    Arch, CachyOS:  sudo pacman -S --needed libxkbcommon libxkbcommon-x11 libxcb" >&2
      echo "    Debian, Ubuntu: sudo apt install libxkbcommon0 libxkbcommon-x11-0 libxcb-xkb1" >&2
      echo "    Fedora:         sudo dnf install libxkbcommon libxkbcommon-x11 libxcb" >&2
      ;;
  esac
  return 0
}

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

check_libraries
echo "installed into $PREFIX:"
for f in "$HERE"/bin/*; do echo "  $PREFIX/bin/$(basename "$f")"; done
case ":$PATH:" in
  *":$PREFIX/bin:"*) ;;
  *) echo "note: $PREFIX/bin is not on your PATH; add it to run the command-line tools by name" ;;
esac
