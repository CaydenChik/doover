#!/usr/bin/env bats
# S10 — restore failure arms and the nested DOOVER_HOME, end to end through
# the REAL binary (2026-08-15 dive + review). Uses the debug-only single-shot
# failure-injection markers (.doover-test-restore-*-fail in the store root) —
# the same mechanism the unit suites pin, here proven through the CLI.

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

agent_runs() { # $1 tool_use_id, $2 command
  printf '{"session_id":"e2e","cwd":"%s","hook_event_name":"PreToolUse","tool_name":"Bash","tool_use_id":"%s","tool_input":{"command":%s}}' \
    "$PROJ" "$1" "$(printf '%s' "$2" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')" \
    | "$DOOVER_BIN" hook pre
  ( cd "$PROJ" && bash --noprofile --norc -c "$2" )
  printf '{"session_id":"e2e","cwd":"%s","hook_event_name":"PostToolUse","tool_name":"Bash","tool_use_id":"%s","duration_ms":5,"tool_input":{"command":%s},"tool_response":{"stdout":"","stderr":"","interrupted":false}}' \
    "$PROJ" "$1" "$(printf '%s' "$2" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')" \
    | "$DOOVER_BIN" hook post
}

opaque_rig() {
  # an unregistered script forces the unknown policy's full-cwd snapshot,
  # with a name-only-skippable node_modules holding never-captured data
  mkdir -p "$PROJ/node_modules"
  echo "NEVER-CAPTURED" > "$PROJ/node_modules/dep.js"
  echo "precious" > "$PROJ/data.txt"
  printf '#!/bin/bash\nrm -f data.txt\n' > "$PROJ/nuke.sh"
  chmod +x "$PROJ/nuke.sh"
  agent_runs t1 './nuke.sh'
  [ ! -f "$PROJ/data.txt" ]
}

@test "S10a: undo with DOOVER_HOME nested in the project preserves the live home" {
  export DOOVER_HOME="$PROJ/.doover"
  opaque_rig
  run "$DOOVER_BIN" undo
  [ "$status" -eq 0 ]
  [ "$(cat "$PROJ/data.txt")" = "precious" ]
  # the live nested home rode the swap: the journal still answers
  run "$DOOVER_BIN" log
  [ "$status" -eq 0 ]
  [[ "$output" == *"undo of action"* ]]
}

@test "S10b: a failed swap moves carried dirs back and a retry converges" {
  opaque_rig
  touch "$DOOVER_HOME/store/.doover-test-restore-swap-fail"
  run "$DOOVER_BIN" undo
  [ "$status" -ne 0 ]
  # the never-captured live dir was moved back, not deleted with staging
  [ "$(cat "$PROJ/node_modules/dep.js")" = "NEVER-CAPTURED" ]
  # marker is single-shot: the retry converges. The disturbed target no
  # longer matches the action's post-state, so the bare retry must first
  # report the conflict (exit 3) — then --force converges, exactly the
  # two-step recovery the error message prescribes.
  run "$DOOVER_BIN" undo
  [ "$status" -eq 3 ]
  run "$DOOVER_BIN" undo --force
  [ "$status" -eq 0 ]
  [ "$(cat "$PROJ/data.txt")" = "precious" ]
  [ "$(cat "$PROJ/node_modules/dep.js")" = "NEVER-CAPTURED" ]
}

@test "S10c: a failed move-back preserves staging, names it, and journals it" {
  opaque_rig
  touch "$DOOVER_HOME/store/.doover-test-restore-swap-fail"
  touch "$DOOVER_HOME/store/.doover-test-restore-moveback-fail"
  run "$DOOVER_BIN" undo
  [ "$status" -ne 0 ]
  [[ "$output" == *"LIVE data"* ]]
  staging="$(find "$JAIL" -maxdepth 2 -name '.doover-restore-*' -type d | head -1)"
  [ -n "$staging" ]
  [ "$(cat "$staging/node_modules/dep.js")" = "NEVER-CAPTURED" ]
  # durable record: the action carries a note naming the failure
  run "$DOOVER_BIN" show 1
  [[ "$output" == *"left live data"* ]]
}
