#!/bin/bash
# DDEV + Drupal CMS on the current Docker context: the workflow a PHP
# developer actually runs. Bind mounts only (performance mode "none", no
# Mutagen), so the Mac file sharing is what's measured.
#
#   bench/ddev-drupal.sh <label>      e.g. orbstack, berth
#
# Prints one markdown table row per run. The project lives under
# ~/ddev-bench/<label> (the project takes that name) and is deleted afterwards.
set -euo pipefail
exec </dev/null   # ddev may otherwise wait on a prompt
label=${1:?label}
dir=$HOME/ddev-bench/$label
now() { python3 -c 'import time; print(time.time())'; }
since() { python3 -c "import sys; print(f'{float(sys.argv[2]) - float(sys.argv[1]):.1f}')" "$1" "$(now)"; }

rm -rf "$dir"; mkdir -p "$dir"; cd "$dir"
ddev config --project-type=drupal11 --docroot=web >/dev/null
# Drupal CMS ships its own .ddev/config.yaml; local overrides survive it.
printf 'performance_mode: none\n' > .ddev/config.local.yaml

t=$(now); ddev start -y >/dev/null 2>&1; start=$(since "$t")
t=$(now); ddev composer create-project -y drupal/cms >/dev/null 2>&1; create=$(since "$t")
# Same dependencies from the composer cache: almost only small-file writes.
rm -rf vendor web/core web/modules/contrib web/themes/contrib web/profiles/contrib web/recipes 2>/dev/null || true
t=$(now); ddev composer install >/dev/null 2>&1; install=$(since "$t")
t=$(now); ddev drush site:install --yes >/dev/null 2>&1; siteinstall=$(since "$t")

# Straight to the web container's IP (both have routable container IPs):
# measures PHP on the shared files, not DDEV's router or port forwarding.
web() { docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}' "ddev-$label-web" | awk '{print $1}'; }
# A random query string bypasses Drupal's page cache: every request runs PHP.
ttfb() { curl -s -o /dev/null -w '%{time_total}' -H "Host: $label.ddev.site" "http://$(web)/?nocache=$RANDOM$RANDOM"; }
ddev restart >/dev/null 2>&1   # cold opcache
cold=$(python3 -c "print(f'{$(ttfb) * 1000:.0f}')")   # first page after the restart
code=$(curl -s -o /dev/null -w '%{http_code}' -H "Host: $label.ddev.site" "http://$(web)/user/login")
[ "$code" = 200 ] || echo "warning: login page returned $code" >&2
for _ in $(seq 10); do ttfb >/dev/null; done
warm=$(for _ in $(seq 20); do ttfb; echo; done | python3 -c 'import sys; v=sorted(float(l) for l in sys.stdin if l.strip()); print(f"{v[len(v)//2]*1000:.0f}")')
t=$(now); ddev restart >/dev/null 2>&1; restart=$(since "$t")
files=$(find "$dir" -type f | wc -l | tr -d ' ')

echo "| $label | $start s | $create s | $install s | $siteinstall s | $cold ms | $warm ms | $restart s | $files |"
ddev delete -Oy >/dev/null 2>&1
cd /; rm -rf "$dir"
