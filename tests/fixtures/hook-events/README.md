# Golden hook-event fixtures

Real Claude Code `PreToolUse`/`PostToolUse` JSON payloads, captured live from
**Claude Code v2.1.201** (2026-07-06) via a tee-style capture hook in a
throwaway project, then scrubbed. They pin the harness contract: if Claude
Code changes its hook schema, `hook_fixtures.rs` breaks loudly instead of
doover misparsing events in the field.

## Contract findings from the live capture (design-relevant)

1. **`cwd` tracks the session's live working directory across calls.** After
   `cd cache-dir && rm a.tmp`, the *next* PreToolUse arrives with
   `cwd=.../cache-dir` (and the cd-command's own PostToolUse already shows the
   new cwd). Cross-call cwd tracking is the harness's job; the resolver only
   tracks `cd` *within* one command line.
2. **Failed commands produce NO PostToolUse.** `false` fired PreToolUse only —
   PostToolUse runs on success. The journal (step 4) must treat a pending
   action with no post event as failed/uncertain, closed out at the next event
   or session end — never assume a post will arrive.
3. **There is no exit code anywhere.** `tool_response` carries
   `stdout`/`stderr`/`interrupted`/`isImage`/`noOutputExpected`, plus a
   top-level `duration_ms`. The spec's original "record exit code" plan is
   amended: record duration + stderr presence; success is implied by the post
   event existing at all.
4. Heredocs arrive as a single `command` string with literal `\n`s; unicode is
   plain JSON-escaped text. Both as assumed.
5. Extra fields we get for free: `prompt_id`, `tool_use_id` (correlates
   pre/post pairs), `permission_mode`.

## Capture & scrub procedure (reproducible)

1. Throwaway project with `.claude/settings.json` hooks (PreToolUse +
   PostToolUse, matcher `Bash`) appending `TAG\t<stdin JSON>` lines to
   `events.jsonl` via a tiny script.
2. `claude -p "<run these commands as separate Bash calls…>" --allowedTools Bash`
   in that directory.
3. Scrub: capture dir → `/Users/tester/project`, session/prompt/tool_use ids →
   fixed placeholder values, `transcript_path` → placeholder, `duration_ms` →
   42. Command text and structure are preserved verbatim.
