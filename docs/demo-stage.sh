# staging for the REAL-session demo tape
export PATH="$HOME/.cargo/bin:$PATH"
JAIL="$(mktemp -d /tmp/demo.XXX)"
export DOOVER_HOME="$JAIL/home"
mkdir -p "$JAIL/proj/dist" "$JAIL/proj/photos"
cd "$JAIL/proj"
echo 'console.log("build artifact");' > dist/bundle.js
head -c 4096 /dev/urandom > photos/birthday.jpg
head -c 4096 /dev/urandom > photos/wedding.jpg
shasum photos/*.jpg > sums.before
# real hooks, exactly what `doover init --project` writes
mkdir -p .claude
cat > .claude/settings.json <<JSON
{
  "hooks": {
    "PreToolUse":  [{"matcher": "Bash", "hooks": [{"type": "command", "command": "$HOME/.cargo/bin/doover hook pre",  "timeout": 20}]}],
    "PostToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "$HOME/.cargo/bin/doover hook post", "timeout": 20}]}]
  }
}
JSON
CLAUDE_BIN="$(ls -d $HOME/.vscode/extensions/anthropic.claude-code-*/resources/native-binary/claude 2>/dev/null | sort -V | tail -1)"
claude() { "$CLAUDE_BIN" "$@"; }
