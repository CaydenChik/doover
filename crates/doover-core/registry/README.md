# Reversibility registry (data is CC0-1.0)

Machine-readable classifications of shell commands and agent tool calls: effect class
(`safe | mutating | destructive | externalizing | irreversible | unknown`), how to
extract the affected paths from arguments, and the undo strategy.

Vocabulary aligns with MCP tool annotations (`readOnlyHint`/`destructiveHint`) so the
data can feed other tools, not just doover. Schema and effect-handling matrix:
`doover-mvp-spec.md` §4.5. Entries arrive in build step 1.

Everything in this directory is dedicated to the public domain under CC0-1.0
(see LICENSE in this directory) to maximize reuse by other safety tools.
