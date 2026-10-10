#!/bin/sh
# Generate one piece of scene art with Grok (xAI) and save it as PNG:
#
#   GROK_API_KEY=... tools/grok_image.sh assets/bg_night.png 3:2 "prompt text"
#
# Every background and sprite under assets/ that is not drawn from primitives
# came out of this call. Backgrounds are 3:2 or 4:3 landscape with a flat
# sidewalk along the bottom fifth (src/world.lua pins the ground line there);
# sprites are 1:1 on a solid magenta screen that src/assets.lua knocks out.
set -eu

out=${1:?usage: grok_image.sh <out.png> <aspect_ratio> <prompt>}
aspect=${2:?aspect ratio, e.g. 3:2}
shift 2
prompt=$*
model=${GROK_IMAGE_MODEL:-grok-imagine-image}

[ -n "${GROK_API_KEY:-}" ] || { echo "GROK_API_KEY is not set" >&2; exit 1; }

body=$(python3 - "$model" "$aspect" "$prompt" <<'PY'
import json, sys
model, aspect, prompt = sys.argv[1:4]
print(json.dumps({
    "model": model,
    "prompt": prompt,
    "n": 1,
    "response_format": "b64_json",
    "aspect_ratio": aspect,
}))
PY
)

tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
curl -sS -m 300 https://api.x.ai/v1/images/generations \
  -H "Authorization: Bearer $GROK_API_KEY" \
  -H "Content-Type: application/json" \
  -d "$body" -o "$tmp"

python3 - "$tmp" "$out" <<'PY'
import base64, json, sys
src, out = sys.argv[1:3]
d = json.load(open(src))
if "data" not in d:
    sys.stderr.write(json.dumps(d, indent=1) + "\n")
    sys.exit(1)
open(out, "wb").write(base64.b64decode(d["data"][0]["b64_json"]))
print(out)
PY
