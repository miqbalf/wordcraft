#!/usr/bin/env bash
# Fork-only (udinCraft release workflow): checks out the optional build inputs beside the repo and
# exports them for the later steps of the job.
#   CRAFT_FONTS_REF set   storytold/craft-fonts at that commit in ../craft-fonts (CRAFT_FONTS_DIR,
#                         CRAFT_FONTS_REQUIRED=1)
#   craft-i18n.rev there  storytold/craft-i18n at that commit in ../craft-i18n (CRAFT_I18N_DIR,
#                         CRAFT_I18N_REQUIRED=1)
set -euo pipefail

fetch() { # <owner/repo> <commit> <dir>
  git init -q "$3"
  git -C "$3" fetch -q --depth 1 "https://github.com/$1" "$2"
  git -C "$3" checkout -q FETCH_HEAD
  echo "$1 $(git -C "$3" rev-parse HEAD) in $3"
}
# Windows runners: cargo and PowerShell want D:\... paths, not /d/...
native() { if command -v cygpath >/dev/null; then cygpath -w "$1"; else printf '%s' "$1"; fi; }

parent="$(cd .. && pwd)"
if [[ -n "${CRAFT_FONTS_REF:-}" ]]; then
  fetch storytold/craft-fonts "$CRAFT_FONTS_REF" "$parent/craft-fonts"
  { echo "CRAFT_FONTS_DIR=$(native "$parent/craft-fonts")"; echo "CRAFT_FONTS_REQUIRED=1"; } >>"$GITHUB_ENV"
fi
if [[ -f craft-i18n.rev ]]; then
  fetch storytold/craft-i18n "$(tr -d '[:space:]' <craft-i18n.rev)" "$parent/craft-i18n"
  { echo "CRAFT_I18N_DIR=$(native "$parent/craft-i18n")"; echo "CRAFT_I18N_REQUIRED=1"; } >>"$GITHUB_ENV"
fi
