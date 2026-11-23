#!/usr/bin/env bash
# Builds the GitHub Pages site into _site (or $1): the static files in site/, a sitemap dated by the
# last commit that touched the site or the docs, and llms-full.txt (README and Docker docs in one
# file, for LLM crawlers). Used by .github/workflows/pages.yml; run it locally to preview.
set -euo pipefail
cd "$(dirname "$0")/.."
out="${1:-_site}"
base="https://strise.github.io/spanbolt"

rm -rf "$out"
mkdir -p "$out"
cp -R site/. "$out/"

lastmod="$(git log -1 --format=%cs -- site README.md docs 2>/dev/null || true)"
lastmod="${lastmod:-$(date -u +%F)}"
cat > "$out/sitemap.xml" <<XML
<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url><loc>$base/</loc><lastmod>$lastmod</lastmod></url>
  <url><loc>$base/llms.txt</loc><lastmod>$lastmod</lastmod></url>
  <url><loc>$base/llms-full.txt</loc><lastmod>$lastmod</lastmod></url>
</urlset>
XML

{
  echo "# spanbolt: full documentation for LLMs"
  echo
  echo "Source: https://github.com/strise/spanbolt (generated from README.md and docs/docker.md)."
  for f in README.md docs/docker.md; do
    echo
    echo "---"
    echo
    echo "<!-- $f -->"
    echo
    cat "$f"
  done
} > "$out/llms-full.txt"

echo "site built in $out"
