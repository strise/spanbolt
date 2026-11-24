#!/usr/bin/env bash
# Renders the link preview images from site/og/card.html with headless Chrome:
# site/og.png (1200x630, og:image / twitter:image) and site/og/github-social.png (1280x640, upload
# under the repository's Settings → Social preview). Needs network for Google Fonts.
set -euo pipefail
cd "$(dirname "$0")/.."
chrome="${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"
render() {
  "$chrome" --headless=new --disable-gpu --hide-scrollbars --force-device-scale-factor=1 \
    --virtual-time-budget=5000 --window-size="$1,$2" --screenshot="$3" \
    "file://$PWD/site/og/card.html?w=$1&h=$2" >/dev/null 2>&1
  echo "$3"
}
render 1200 630 site/og.png
render 1280 640 site/og/github-social.png
