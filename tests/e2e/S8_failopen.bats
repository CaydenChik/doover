#!/usr/bin/env bats
# S8 — fail-open: NOTHING doover does may block the agent. Every failure mode
# of the hook binary must exit 0 (the harness proceeds), warn on stderr, and
# leave no partial state that breaks later invocations.

setup() {
  [ -n "$DOOVER_BIN" ] || { echo "DOOVER_BIN not set (run via 'make e2e')" >&2; return 1; }
  JAIL="$(mktemp -d)"
  export HOME="$JAIL/home"
  export DOOVER_HOME="$JAIL/doover-home"
  mkdir -p "$HOME" "$JAIL/proj/build"
  echo "precious" > "$JAIL/proj/build/a.txt"
  cd "$JAIL"
}

teardown() {
  rm -rf "$JAIL"
}

# write a pre-event JSON to $JAIL/event.json (avoids function-in-subshell
# variable-scope pitfalls under bats `run`)
write_pre_event() {
  printf '{"session_id":"e2e-s1","cwd":"%s","hook_event_name":"PreToolUse","tool_name":"Bash","tool_use_id":"%s","tool_input":{"command":"%s"}}' \
    "$JAIL/proj" "$1" "$2" > "$JAIL/event.json"
}

@test "S8: garbage stdin exits 0 and warns" {
  run bash -c "echo 'not json at all' | \"$DOOVER_BIN\" hook pre"
  [ "$status" -eq 0 ]
  [[ "$output" == *"fail-open"* ]]
}

@test "S8: empty stdin exits 0 and warns" {
  run bash -c "printf '' | \"$DOOVER_BIN\" hook pre"
  [ "$status" -eq 0 ]
  [[ "$output" == *"fail-open"* ]]
}

@test "S8: an internal panic exits 0 and warns" {
  run bash -c "DOOVER_TEST_PANIC=1 \"$DOOVER_BIN\" hook pre < /dev/null"
  [ "$status" -eq 0 ]
  [[ "$output" == *"panicked (fail-open"* ]]
}

@test "S8: unwritable DOOVER_HOME exits 0 and warns" {
  export DOOVER_HOME="$JAIL/blocked/doover-home"
  mkdir -p "$JAIL/blocked"
  chmod 555 "$JAIL/blocked"
  write_pre_event t1 'rm -rf build'
  run bash -c "\"$DOOVER_BIN\" hook pre < \"$JAIL/event.json\""
  chmod 755 "$JAIL/blocked"
  [ "$status" -eq 0 ]
}

@test "S8: a real destructive pre-event journals and snapshots, exit 0" {
  write_pre_event t1 'rm -rf build'
  run bash -c "\"$DOOVER_BIN\" hook pre < \"$JAIL/event.json\""
  [ "$status" -eq 0 ]
  [ -f "$DOOVER_HOME/journal.db" ]
  # the store gained at least one object (build/a.txt content)
  [ -n "$(find "$DOOVER_HOME/store/objects" -type f 2>/dev/null)" ]
}

@test "S8: failure paths leave the next invocation fully functional" {
  run bash -c "echo garbage | \"$DOOVER_BIN\" hook pre"
  [ "$status" -eq 0 ]
  write_pre_event t2 'rm -rf build'
  run bash -c "\"$DOOVER_BIN\" hook pre < \"$JAIL/event.json\""
  [ "$status" -eq 0 ]
  [ -f "$DOOVER_HOME/journal.db" ]
}
