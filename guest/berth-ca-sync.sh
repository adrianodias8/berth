#!/bin/sh
# berth: put the CA certificates the Mac trusts (src/cacerts.rs writes the
# bundle on the home share) into Debian's trust store. Runs before Docker
# starts (a docker.service drop-in), so pulls and pushes verify against them;
# the agent installs this file with the bundle's path filled in.
src="@BUNDLE@"
dir=/usr/local/share/ca-certificates/berth
# The home share may still be mounting this early in boot.
i=0
while [ ! -d "$(dirname "$src")" ] && [ $i -lt 20 ]; do sleep 0.5; i=$((i + 1)); done
new=$(cat "$src" 2>/dev/null)
[ "$new" = "$(cat "$dir/.bundle" 2>/dev/null)" ] && exit 0
rm -rf "$dir"
mkdir -p "$dir"
if [ -n "$new" ]; then
    awk -v d="$dir" '/BEGIN CERTIFICATE/ { n++; f = sprintf("%s/mac-%03d.crt", d, n) } f { print > f }' "$src"
fi
printf '%s' "$new" > "$dir/.bundle"
update-ca-certificates --fresh >/dev/null 2>&1 || true
