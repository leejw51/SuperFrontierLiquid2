#!/bin/sh
# Generate every retro asset with Grok. Sprites come back on solid magenta
# and knock_out.py turns that transparent.
set -eu
cd "$(dirname "$0")/.."
G=tools/grok_image.sh
STYLE="16-bit pixel art, SNES and Sega Genesis era, chunky visible pixels, limited palette, crisp edges, no anti-aliasing, no text, no watermark"
SPR="$STYLE, single sprite centred on a solid flat magenta (#FF00FF) background, nothing else in frame"
gen() { out=$1; ar=$2; shift 2; [ -f "$out" ] && { echo "skip $out"; return; }; $G "$out" "$ar" "$*" || echo "FAILED $out"; }

gen art-src/hero_city.png 16:9 "$STYLE, Causeway Bay Hong Kong at night, dense neon signs, tram, skyscrapers, purple and teal sky, wide cityscape background for a video game title screen"
gen art-src/title_logo.png 3:2 "$STYLE, video game title logo artwork: a glowing gold pixel coin with a lightning bolt, a chunky candlestick chart rising behind it, dark navy background, no letters"
gen art-src/coin_btc.png 1:1 "$SPR, a golden coin with a stylised B symbol"
gen art-src/coin_eth.png 1:1 "$SPR, a silver-blue coin with a diamond crystal symbol"
gen art-src/coin_sol.png 1:1 "$SPR, a purple and teal gradient coin with three diagonal bars"
gen art-src/coin_usd.png 1:1 "$SPR, a green paper banknote stack with a dollar sign"
gen art-src/stock_cert.png 1:1 "$SPR, a cream stock certificate scroll with a red wax seal and a tiny rising chart"
gen art-src/mascot_trader.png 1:1 "$SPR, a cheerful trader character with sunglasses and a headset holding a handheld console, JRPG portrait style"
gen art-src/mascot_admin.png 1:1 "$SPR, a stern arcade operator character with a cap and a wrench, holding a clipboard, JRPG portrait style"
gen art-src/faucet.png 1:1 "$SPR, a brass water tap pouring gold coins"
gen art-src/trophy.png 1:1 "$SPR, a gold trophy cup with a star"
gen art-src/block.png 1:1 "$SPR, a glowing teal cube block with circuit lines, blockchain block"
gen art-src/bull.png 1:1 "$SPR, a green charging bull, side view"
gen art-src/bear.png 1:1 "$SPR, a red growling bear, side view"
gen art-src/rocket.png 1:1 "$SPR, a small white and red rocket ship with flame"
gen art-src/skull.png 1:1 "$SPR, a small red pixel skull with crossed bones, liquidation warning icon"
gen art-src/robot.png 1:1 "$SPR, a small boxy trading robot with an antenna and a screen face, bot icon"
gen art-src/panel_frame.png 1:1 "$STYLE, an ornate square 16-bit RPG dialogue box frame, dark navy fill, gold and cream bevelled pixel border with corner studs, empty in the middle"
python3 tools/knock_out.py
