#!/usr/bin/env bash
set -euo pipefail

mountpoint=${1:?mountpoint required}
remote=${2:-}
file="$mountpoint/.mobfs-chaos-$(date +%s).txt"

printf 'mobfs chaos start\n'
printf 'mountpoint=%s\n' "$mountpoint"

dd if=/dev/zero of="$file" bs=1m count=8 status=none
sync
printf 'big-write ok\n'

for i in $(seq 1 50); do
  tmp="$file.tmp"
  printf 'save-%s-%s\n' "$i" "$(date +%s%N)" > "$tmp"
  mv "$tmp" "$file"
done
printf 'repeated-editor-save ok\n'

if [[ -n "$remote" ]]; then
  mobfs run sh -lc 'pwd && git status --short || true'
  printf 'remote-run ok\n'
fi

printf 'now manually test: Wi-Fi off/on, sleep/wake, SSH tunnel death, daemon kill, laptop IP change\n'
printf 'then rerun: mobfs status && mobfs run test -f %q\n' "$file"
