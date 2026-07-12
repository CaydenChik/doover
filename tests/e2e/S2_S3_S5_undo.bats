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

@test "S3c: LAUNCH CLAIM — a realistic tree restores byte-identical (diff -r)" {
  # nested dirs, an executable, a symlink, a larger file, dot-files —
  # the restoration must be indistinguishable from the original
  mkdir -p "$PROJ/app/src/utils" "$PROJ/app/assets"
  echo 'fn main() {}' > "$PROJ/app/src/main.rs"
  echo 'pub fn helper() {}' > "$PROJ/app/src/utils/helper.rs"
  printf '#!/bin/sh\necho hi\n' > "$PROJ/app/build.sh"; chmod 755 "$PROJ/app/build.sh"
  head -c 262144 /dev/urandom > "$PROJ/app/assets/blob.bin"
  echo 'secret=1' > "$PROJ/app/.env"
  ln -s src/main.rs "$PROJ/app/entry.rs"
  cp -R "$PROJ/app" "$JAIL/reference"

  agent_runs t9 'rm -rf app'
  [ ! -e "$PROJ/app" ]

  run "$DOOVER_BIN" undo
  [ "$status" -eq 0 ]
  # byte-identical tree, symlinks compared as links
  run diff -r "$JAIL/reference" "$PROJ/app"
  [ "$status" -eq 0 ]
  # executable bit survived
  [ -x "$PROJ/app/build.sh" ]
  # symlink is a symlink pointing at the same target
  [ "$(readlink "$PROJ/app/entry.rs")" = "src/main.rs" ]
}

@test "S9: 8 concurrent agent sessions + concurrent gc keep the journal coherent" {
  # the D5 stress check: parallel hook processes hammering one DOOVER_HOME
  # while gc runs concurrently — no lost actions, no deadlock, journal sane
  for i in 1 2 3 4 5 6 7 8; do
    (
      mkdir -p "$PROJ/w$i"
      echo "data $i" > "$PROJ/w$i/f.txt"
      printf '{"session_id":"con-%s","cwd":"%s","hook_event_name":"PreToolUse","tool_name":"Bash","tool_use_id":"t%s","tool_input":{"command":"rm -rf w%s"}}' \
        "$i" "$PROJ" "$i" "$i" | "$DOOVER_BIN" hook pre
      rm -rf "$PROJ/w$i"
      printf '{"session_id":"con-%s","cwd":"%s","hook_event_name":"PostToolUse","tool_name":"Bash","tool_use_id":"t%s","duration_ms":3,"tool_input":{"command":"rm -rf w%s"},"tool_response":{"stdout":"","stderr":"","interrupted":false}}' \
        "$i" "$PROJ" "$i" "$i" | "$DOOVER_BIN" hook post
    ) &
  done
  "$DOOVER_BIN" gc >/dev/null 2>&1 &
  wait

  # every action landed and completed; the journal passes integrity
  run "$DOOVER_BIN" log -n 50
  [ "$status" -eq 0 ]
  for i in 1 2 3 4 5 6 7 8; do
    [[ "$output" == *"rm -rf w$i"* ]]
  done
  [[ "$output" != *"pending"* ]]
  run "$DOOVER_BIN" doctor
  [ "$status" -eq 0 ]
  [[ "$output" == *"journal integrity"* ]]
  # and one of the concurrent actions is actually undoable
  run "$DOOVER_BIN" undo
  [ "$status" -eq 0 ]
}
