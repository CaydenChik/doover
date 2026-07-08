#!/usr/bin/env bats
# S7 — init installs working hooks, and gc respects retention, through the
# real binary.

setup() {
  [ -n "$DOOVER_BIN" ] || { echo "DOOVER_BIN not set" >&2; return 1; }
  JAIL="$(mktemp -d)"
  export HOME="$JAIL/home"
  export DOOVER_HOME="$JAIL/doover-home"
  PROJ="$JAIL/proj"
  mkdir -p "$HOME" "$PROJ"
  cd "$PROJ"
}

teardown() { rm -rf "$JAIL"; }

@test "S7: init --project writes a valid settings.json with both hooks" {
  run "$DOOVER_BIN" init --project
  [ "$status" -eq 0 ]
  [ -f "$PROJ/.claude/settings.json" ]
  python3 -c "import json,sys; json.load(open('$PROJ/.claude/settings.json'))"  # valid JSON
  grep -q "doover hook pre" "$PROJ/.claude/settings.json"
  grep -q "doover hook post" "$PROJ/.claude/settings.json"
}

@test "S7: doctor reports hooks-installed after a global init" {
  run "$DOOVER_BIN" init
  [ "$status" -eq 0 ]
  [ -f "$HOME/.claude/settings.json" ]
  run "$DOOVER_BIN" doctor
  [ "$status" -eq 0 ]
  [[ "$output" == *"hooks installed"* ]]
}

@test "S7: status reflects a journaled action" {
  printf '{"session_id":"s","cwd":"%s","hook_event_name":"PreToolUse","tool_name":"Bash","tool_use_id":"t1","tool_input":{"command":"ls"}}' "$PROJ" \
    | "$DOOVER_BIN" hook pre
  run "$DOOVER_BIN" status
  [ "$status" -eq 0 ]
  [[ "$output" == *"1 pending"* ]] || [[ "$output" == *"actions:"* ]]
}

@test "S7: gc dry-run reports without deleting; real gc frees nothing recent" {
  echo precious > "$PROJ/keep.txt"
  printf '{"session_id":"s","cwd":"%s","hook_event_name":"PreToolUse","tool_name":"Bash","tool_use_id":"t1","tool_input":{"command":"rm keep.txt"}}' "$PROJ" \
    | "$DOOVER_BIN" hook pre
  # store gained an object
  [ -n "$(find "$DOOVER_HOME/store/objects" -type f 2>/dev/null)" ]

  run "$DOOVER_BIN" gc --dry-run
  [ "$status" -eq 0 ]
  [[ "$output" == *"would free"* ]]
  # recent action: nothing actually removed
  [ -n "$(find "$DOOVER_HOME/store/objects" -type f 2>/dev/null)" ]

  run "$DOOVER_BIN" gc
  [ "$status" -eq 0 ]
  [ -n "$(find "$DOOVER_HOME/store/objects" -type f 2>/dev/null)" ]  # recent, kept
}
