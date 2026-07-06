# Golden hook-event fixtures

Real captured Claude Code `PreToolUse` / `PostToolUse` JSON payloads, recorded once
from a live session and committed here (build step 5). They pin the harness contract:
if Claude Code changes its hook schema, the T6 adapter tests break loudly instead of
doover failing silently in the field.

Capture procedure (documented for reproducibility): configure a hook whose command is
`tee tests/fixtures/hook-events/<name>.json`, run the scenario in a throwaway project,
then scrub absolute paths and session ids to fixture values.
