#!/bin/bash
# usage: bash tools/gen.sh <out-name> <base-image> "<expression change>"
# Generates an expression variant via Codex built-in image tool. Result: source/<out-name>.png
cd "$(dirname "$0")/.."
codex exec -s workspace-write --skip-git-repo-check --image "$2" -- "Edit the attached image with your built-in image generation tool (the subscription based image_gen tool of this session). Do NOT use the \$imagegen CLI script, do NOT use OPENAI_API_KEY or any API calls, do NOT edit it with code. This is one frame of a set of expression variants of the same cartoon character, so it must line up with the original: keep exactly the same character, same painterly gouache style, same colors, same body, same pose, same size, same position and framing in the canvas, same fully transparent background (PNG with alpha). Change: $3. Keep everything else unchanged. Output 1024x1024. Save the result as $1.png in the current directory. If the built-in image generation tool is unavailable, reply exactly IMAGEGEN_UNAVAILABLE." 2>&1 | tail -n 3
[ -f "$1.png" ] && mv "$1.png" "source/$1.png" && echo "OK source/$1.png"
