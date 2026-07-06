# Third-party notice and clean-room policy

## Licensed material we use or vendor

- `binpash/try` (MIT) — the v0.2 Linux overlayfs backend will vendor logic from this
  project with attribution and license preservation.
- `anthropic-experimental/sandbox-runtime` (Apache-2.0) — planned phase-2 dependency
  for fail-closed scope enforcement.

## Reference-only projects — DO NOT COPY CODE

The following projects have **no usable open-source license** (all rights reserved by
default). Their observable behavior and documentation may be studied; their code must
never be copied, ported, or closely paraphrased into this repository:

- `RonitSachdev/ccundo` — license: NOASSERTION
- `A386official/diffback` — license: none published

If a contribution appears derived from either, reject it and reimplement clean-room.

## Methodology citations

Doover's design follows published research: Atomix (arXiv:2602.14849, effect
taxonomy), Parallax (arXiv:2604.12986, snapshot-before-destructive), YoloFS
(arXiv:2604.13536, append-only history), enclawed (arXiv:2604.16838, inverse
registration), and aligns its registry vocabulary with MCP tool annotations
(spec rev. 2025-03-26).
