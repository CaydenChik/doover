#!/usr/bin/env bash
# Delta-minimize a tree-sitter-bash crasher (fuzz-hunt diagnostic).
# Usage: minimize.sh <crasher-file> <parse_stdin-binary>
set -u
CUR="$(mktemp)"; TRY="$(mktemp)"
cp "$1" "$CUR"
BIN="$2"

crashes() { # returns 0 if the candidate crashes the parser
  timeout 10 "$BIN" < "$1" > /dev/null 2>&1
  [ $? -ge 128 ]
}

if ! crashes "$CUR"; then
  echo "input does not crash the parser on this platform"; exit 1
fi

len=$(wc -c < "$CUR")
chunk=$(( len / 2 ))
while [ "$chunk" -ge 1 ]; do
  reduced=0
  off=0
  len=$(wc -c < "$CUR")
  while [ "$off" -lt "$len" ]; do
    head -c "$off" "$CUR" > "$TRY"
    tail -c +"$(( off + chunk + 1 ))" "$CUR" >> "$TRY"
    if [ -s "$TRY" ] && crashes "$TRY"; then
      cp "$TRY" "$CUR"
      len=$(wc -c < "$CUR")
      reduced=1
    else
      off=$(( off + chunk ))
    fi
  done
  if [ "$reduced" -eq 0 ]; then
    chunk=$(( chunk / 2 ))
  fi
done

echo "=== minimized crasher ($(wc -c < "$CUR") bytes) ==="
xxd "$CUR"
echo "=== printf-reproducer ==="
printf 'printf %%b '\''%s'\'' | parse_stdin\n' "$(od -An -v -t x1 "$CUR" | tr -d ' \n' | sed 's/../\\x&/g')"
