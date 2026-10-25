#!/bin/bash
# composer install of Drupal CMS into a bind mount, from a warm Composer
# cache (a volume): almost only small-file writes on the Mac share, without
# DDEV around it.
#
#   bench/fs/composer.sh [context] [runs]
set -euo pipefail
ctx=${1:-berth} runs=${2:-3}
dir=$HOME/berth-composer.noindex   # .noindex: Spotlight skips it
d() { docker --context "$ctx" "$@"; }
composer() { d run --rm -v "$dir:/app" -v berth-composer-cache:/tmp/cache -e COMPOSER_CACHE_DIR=/tmp/cache -w /app composer:2 "$@"; }
if [ ! -f "$dir/composer.lock" ]; then
  rm -rf "$dir"; mkdir -p "$dir"
  composer create-project -q --no-install drupal/cms /app/p >/dev/null
  mv "$dir/p/composer.json" "$dir/"; rm -rf "$dir/p"
  composer update -q --no-scripts --ignore-platform-reqs >/dev/null
fi
composer install -q --no-scripts --ignore-platform-reqs >/dev/null   # warms the cache
for _ in $(seq "$runs"); do
  rm -rf "$dir/vendor" "$dir/web"
  s=$(python3 -c 'import time; print(time.time())')
  composer install -q --no-scripts --ignore-platform-reqs
  python3 -c "import time; print(f'{time.time() - $s:.2f} s')"
done
echo "$(find "$dir/vendor" "$dir/web" -type f | wc -l | tr -d ' ') files"
