# Agent workflow fixtures — provenance note (AG-22 / plan.md Task A7; AG-23 / Task A8)

Fake reviewer / investigator processes for
`tests/agent_review_workflow_test.rs`, `tests/agent_review_span_test.rs`,
`tests/agent_investigate_workflow_test.rs`, and
`tests/agent_investigate_span_test.rs`. Each is a hand-written POSIX
`/bin/sh` script driven through the `ReviewerSource::Custom` /
`InvestigatorSource::Custom` / `ReviewerCommand` test seam
(`src/internal/ai/review/launcher.rs`; investigate reuses that seam
verbatim) — they stand in for the real `codex` / `claude` / `opencode`
reviewer CLIs so the AG-22 review run loop (fan-in, bounded sink,
terminal states, cancel cleanup) and the AG-23 investigate loop (strict
round-robin, quorum/max-turns, stall/agent-failure pause, continue,
cancel) are exercised deterministically at L1, with no network, no
credentials, and no real agent binaries.

Constraints (all scripts):

- POSIX `sh` builtins only (`printf`, `read`-free, `[`, arithmetic);
  the sole external program is `/bin/sleep`, called by absolute path
  because reviewers spawn under `env_clear` and these fixtures run with
  an **empty** environment (no `PATH`, no `HOME`).
- No network access, no secrets. The one credential-shaped value
  (`reviewer-success.sh`) is a FAKE key assembled at run time from a
  `sk-%s` format string precisely so no token-shaped literal exists in
  the repository; the workflow test asserts the assembled value never
  survives redaction into `findings.md` or the `*.redacted.log` files.
- Committed with the executable bit (`chmod +x`); the tests additionally
  copy each script to a temp dir and re-apply `0o755` so a checkout that
  drops file modes cannot break the suite.
- Large output is generated at run time (`reviewer-flood.sh` loops), so
  no fixture file approaches the 1 MiB in-repo size guideline
  (`agent.md` §"仓库内 fixture 原则").

| fixture | behaviour | used by |
|---|---|---|
| `reviewer-success.sh` | prints markdown findings (incl. a runtime-assembled fake `sk-` credential and an ANSI escape), exit 0 | success / manifest / redaction scenarios |
| `reviewer-error.sh` | one diagnostic line on stderr, exit 3 | error / partial terminal-state scenarios |
| `reviewer-slow.sh` | `/bin/sleep "$1"` (default 1s) then prints one finding | slow-output capture; long-sleep cancel victim |
| `reviewer-flood.sh` | `$1` lines (default 16384 ≈ 1.06 MiB) of 64-char payload on stdout | 64 KiB sink-cap truncation, sink non-blocking, cancel-during-pending-output stress |
| `reviewer-quiet.sh` | exactly two known finding lines, exit 0 | proves a flooding sibling never starves a quiet reviewer |
| `reviewer-pidfile.sh` | writes `$$` to the file named by `$1`, then `exec /bin/sleep 300` | cancel releases reviewer processes (kill -0 fails afterwards) |
| `investigator-conclude.sh` | prints a CONCLUDING stance (contains `conclude`), exit 0 | AG-23 quorum-reaching + round-robin-order + span scenarios |
| `investigator-continue.sh` | prints a NON-concluding stance (no `conclud` token), exit 0 | AG-23 max-turns + round-robin prior-context scenarios |
| `investigator-secret.sh` | CONCLUDES and emits a runtime-assembled fake `sk-` credential + ANSI escape, exit 0 | AG-23 E8 findings-doc redaction proof (secret/ANSI must not survive) |
| `investigator-silent.sh` | exits 0 with NO stdout | AG-23 stall → `PauseReason::Stalled` (empty successful turn) |

Refresh protocol: these fixtures pin the *engine seam*, not any external
CLI wire format — they only need updating if `ReviewerCommand` /
`run_review` / `run_investigate` semantics change (update
`src/internal/ai/review/` or `src/internal/ai/investigate/` and this
table in the same PR).
