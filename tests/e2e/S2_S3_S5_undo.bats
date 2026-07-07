#!/usr/bin/env bats
# S2/S3/S5 — the launch claim, end to end through the REAL binary:
# hook pre → the destructive command actually runs → hook post → doover undo
# brings the data back. Plus conflict refusal (exit 3) and --force.

setup() {
  [ -n "$DOOVER_BIN" ] || { echo "DOOVER_BIN not set (run via 'make e2e')" >&2; return 1; }
  JAIL="$(mktemp -d)"
  export HOME="$JAIL/home"
  export DOOVER_HOME="$JAIL/doover-home"
  PROJ="$JAIL/proj"
  mkdir -p "$HOME" "$PROJ"
  cd "$PROJ"
}

teardown() {
  rm -rf "$JAIL"
}

# drive one command through pre → bash → post, like the harness would
agent_runs() { # $1 tool_use_id, $2 command
  printf '{"session_id":"e2e","cwd":"%s","hook_event_name":"PreToolUse","tool_name":"Bash","tool_use_id":"%s","tool_input":{"command":%s}}' \
    "$PROJ" "$1" "$(printf '%s' "$2" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')" \
    | "$DOOVER_BIN" hook pre
  ( cd "$PROJ" && bash --noprofile --norc -c "$2" )
  printf '{"session_id":"e2e","cwd":"%s","hook_event_name":"PostToolUse","tool_name":"Bash","tool_use_id":"%s","duration_ms":5,"tool_input":{"command":%s},"tool_response":{"stdout":"","stderr":"","interrupted":false}}' \
    "$PROJ" "$1" "$(printf '%s' "$2" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')" \
    | "$DOOVER_BIN" hook post
}

@test "S2: redirect clobber is undone" {
  echo "original notes" > "$PROJ/notes.txt"
  agent_runs t1 'echo clobbered > notes.txt'
  [ "$(cat "$PROJ/notes.txt")" = "clobbered" ]

  run "$DOOVER_BIN" undo
  [ "$status" -eq 0 ]
  [ "$(cat "$PROJ/notes.txt")" = "original notes" ]
}

@test "S3: glob delete is undone — the canonical demo" {
  mkdir -p "$PROJ/photos"
  echo "wedding" > "$PROJ/photos/wedding.jpg"
  echo "birthday" > "$PROJ/photos/birthday.jpg"
  agent_runs t1 'rm -rf photos'
  [ ! -e "$PROJ/photos" ]

  run "$DOOVER_BIN" undo
  [ "$status" -eq 0 ]
  [ "$(cat "$PROJ/photos/wedding.jpg")" = "wedding" ]
  [ "$(cat "$PROJ/photos/birthday.jpg")" = "birthday" ]
}

@test "S3b: selective undo of an earlier action keeps later work" {
  echo "keep me" > "$PROJ/later.txt"
  echo "victim" > "$PROJ/victim.txt"
  agent_runs t1 'rm victim.txt'
  agent_runs t2 'echo extended >> later.txt'

  # undo the FIRST action by id (found via log); later.txt must keep its edit
  ID=$(run_log_first_destructive)
  run "$DOOVER_BIN" undo "$ID"
  [ "$status" -eq 0 ]
  [ "$(cat "$PROJ/victim.txt")" = "victim" ]
  grep -q extended "$PROJ/later.txt"
}

run_log_first_destructive() {
  "$DOOVER_BIN" log -n 50 | awk '/rm victim/ {gsub("#","",$1); print $1; exit}'
}

@test "S5: conflict is refused with exit 3, then --force restores" {
  echo "v1" > "$PROJ/f.txt"
  agent_runs t1 'echo v2 > f.txt'
  echo "user's own work" > "$PROJ/f.txt"   # user edits AFTER the action

  run "$DOOVER_BIN" undo
  [ "$status" -eq 3 ]
  [[ "$output" == *"changed since"* ]]
  [ "$(cat "$PROJ/f.txt")" = "user's own work" ]

  run "$DOOVER_BIN" undo --force
  [ "$status" -eq 0 ]
  [ "$(cat "$PROJ/f.txt")" = "v1" ]
}

@test "S5b: redo re-applies after undo" {
  echo "before" > "$PROJ/f.txt"
  agent_runs t1 'echo after > f.txt'
  run "$DOOVER_BIN" undo
  [ "$status" -eq 0 ]
  [ "$(cat "$PROJ/f.txt")" = "before" ]

  run "$DOOVER_BIN" redo
  [ "$status" -eq 0 ]
  [ "$(cat "$PROJ/f.txt")" = "after" ]
}

@test "S2b: dry-run plans without restoring" {
  echo "original" > "$PROJ/n.txt"
  agent_runs t1 'echo x > n.txt'
  run "$DOOVER_BIN" undo --dry-run
  [ "$status" -eq 0 ]
  [[ "$output" == *"would undo"* ]]
  [ "$(cat "$PROJ/n.txt")" = "x" ]
}
