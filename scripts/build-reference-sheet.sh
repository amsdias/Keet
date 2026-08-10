#!/usr/bin/env bash
# Wrap the Artifact-flavoured reference sheet into a standalone HTML document.
# The published Artifact has no <!doctype>/<head>/<body> (the publisher injects
# them); a file opened straight from disk needs its own or it renders in quirks
# mode. Usage: build-reference-sheet.sh <source.html> [dest.html]
set -euo pipefail
src="${1:?usage: build-reference-sheet.sh <source.html> [dest.html]}"
dest="${2:-docs/reference-sheet.html}"
python3 - "$src" "$dest" <<'PY'
import sys, re
src, dest = sys.argv[1], sys.argv[2]
body = open(src).read()
m = re.search(r'<title>(.*?)</title>\s*', body)
title = m.group(1) if m else "Keet — Reference Sheet"
body = re.sub(r'<title>.*?</title>\s*', '', body, count=1)
open(dest, 'w').write(f"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<meta name="description" content="Reference sheet for Keet - capabilities, command-line switches, in-app controls, presets and config.">
<style>*,*::before,*::after{{box-sizing:border-box}}body{{margin:0}}</style>
</head>
<body>
{body}
</body>
</html>
""")
print(f"wrote {dest}")
PY
