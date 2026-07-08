#!/usr/bin/env bats
# S0 — smoke: binary runs, honest not-implemented behavior, jail hygiene.
#
# E2E safety rules (doover-implementation-plan.md §3):
#   every test runs inside a fresh mktemp jail with HOME overridden;
#   nothing may read or write the real user environment.

setup() {
  [ -n "$DOOVER_BIN" ] || { echo "DOOVER_BIN not set (run via 'make e2e')" >&2; return 1; }
  JAIL="$(mktemp -d)"
  export HOME="$JAIL/home"
  mkdir -p "$HOME"
  cd "$JAIL"
}

teardown() {
  rm -rf "$JAIL"
}

@test "S0: --version exits 0 and prints the crate version" {
  run "$DOOVER_BIN" --version
  [ "$status" -eq 0 ]
  [[ "$output" == *"0.0.1"* ]]
}

# Step 8 removed the last stubs: every subcommand is implemented, so the old
# exit-64 contract is retired. Inspecting a nonexistent action must be a
# clean, specific error — not a crash, not a silent success.
@test "S0: show on an empty journal is a clear error" {
  run "$DOOVER_BIN" show 1
  [ "$status" -eq 1 ]
  [[ "$output" == *"not found"* ]]
}

@test "S0: running doover leaves no droppings in HOME or cwd" {
  run "$DOOVER_BIN" --version
  [ "$status" -eq 0 ]
  # neither the jail HOME (covers XDG paths, ~/.doover) nor the working
  # directory may gain any file from a read-only invocation
  [ -z "$(ls -A "$HOME")" ]
  [ -z "$(ls -A "$JAIL" | grep -v '^home$')" ]
}
