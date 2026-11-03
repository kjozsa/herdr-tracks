#!/bin/sh
# Copies the Tracks mods into the Herdr app (a copy, not a symlink: the app reloads mods only
# on writes inside its own mods folder) and links the binary the mods run onto the PATH.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
home=${HERDR_APP_HOME:-$HOME/.config/herdr-app}
for mod in tracks tracks-diff; do
  [ -L "$home/mods/$mod" ] && rm "$home/mods/$mod"
  mkdir -p "$home/mods/$mod"
  cp "$here/mods/$mod/main.js" "$here/mods/$mod/mod.json" "$home/mods/$mod/"
  herdr-app check "$home/mods/$mod" 2>&1 | grep -v '^WARNING: radv'
done
mkdir -p "$HOME/.local/bin"
ln -sf "$here/../target/release/herdr-tracks" "$HOME/.local/bin/herdr-tracks"
