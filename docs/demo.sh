#!/bin/bash
# The announcement demo, reproducible: runs the exact hook flow Claude Code
# drives, against a throwaway project in a jail. Requires doover on PATH.
set -euo pipefail
JAIL="$(mktemp -d)"; trap 'rm -rf "$JAIL"' EXIT
export DOOVER_HOME="$JAIL/home"
mkdir -p "$JAIL/proj/dist" "$JAIL/proj/photos"; cd "$JAIL/proj"
echo 'console.log("build artifact");' > dist/bundle.js
head -c 4096 /dev/urandom > photos/birthday.jpg
head -c 4096 /dev/urandom > photos/wedding.jpg
ev() { printf '{"session_id":"demo","cwd":"%s","hook_event_name":"%s","tool_name":"Bash","tool_use_id":"t1","tool_input":{"command":"rm -rf dist/ photos/"}%s}' "$PWD" "$1" "$2"; }
ev PreToolUse "" | doover hook pre
shasum photos/*.jpg > "$JAIL/sums.before"
rm -rf dist/ photos/
ev PostToolUse ',"duration_ms":41,"tool_response":{"stdout":"","stderr":"","interrupted":false}' | doover hook post
echo "--- doover log ---";  doover log
echo "--- doover undo ---"; doover undo
echo "--- verify ---";      shasum -c "$JAIL/sums.before"
