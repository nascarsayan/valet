# {{name}}

<!-- Edit this file: what the topic is for, the ticket or PR it serves, how
to run it, and a short summary of the results. -->

## Goal

TODO: one paragraph.
{{run}}
## Layout

| Folder | What goes there | Kept |
|---|---|---|
| `README.md` | this file: goal, how to run, summary of results | committed |
| `Sources/` | code and configs; tools are one Rust crate | committed |
| `Results/` | findings, reports, small outputs worth keeping | committed |
| `Builds/` | build output, run work folders, packages, venvs | reaped |
| `Caches/` | intermediate results reused between runs; safe to delete | reaped |
| `Downloads/` | pinned fetches that no Userland app provides | reaped |
| `Logs/` | run logs | reaped |
| `Scratch/` | temp files of a single run | reaped |

Reaped folders are gitignored, and the daily `valet job` (`valet clean scratch`)
deletes each one after 3 days untouched. Create them only when something
writes there.

## Rules

- Write tools in Rust (`Sources/`, one crate). Shell scripts are thin glue
  only.
- Derive every path from this folder (a crate's `CARGO_MANIFEST_DIR`, a
  script's own directory). Never `$HOME`, `/tmp`, or a tool-specific variable.
- Shared toolchains and caches (Go modules, uv, cargo, helm) come from the
  Userland apps. Do not vendor or re-download them here.
- Every log line starts with an ISO-8601 UTC timestamp and a level.
- Secrets come from the environment (`valet vars`). Never write them into a
  file here.
- Keep `Results/` small: conclusions and the evidence for them, not raw dumps.
