# hidden staging for the demo tape: jail, project, hooks, agent() stand-in
export PATH="$HOME/.cargo/bin:$PATH"
JAIL="$(mktemp -d /tmp/demo.XXX)"
export DOOVER_HOME="$JAIL/home"
mkdir -p "$JAIL/proj/dist" "$JAIL/proj/photos"
cd "$JAIL/proj"
echo 'console.log("build artifact");' > dist/bundle.js
head -c 4096 /dev/urandom > photos/birthday.jpg
head -c 4096 /dev/urandom > photos/wedding.jpg
shasum photos/*.jpg > sums.before
# stands in for your coding agent: runs a command through the SAME
# PreToolUse/PostToolUse hook flow Claude Code drives
agent() {
  local ev='{"session_id":"demo","cwd":"'"$PWD"'","hook_event_name":"%s","tool_name":"Bash","tool_use_id":"t1","tool_input":{"command":"'"$1"'"}%s}'
  printf "$ev" PreToolUse "" | doover hook pre >/dev/null 2>&1
  bash -c "$1"
  printf "$ev" PostToolUse ',"duration_ms":41,"tool_response":{"stdout":"","stderr":"","interrupted":false}' | doover hook post >/dev/null 2>&1
}
