# `libra agent`

Manage external-agent capture for tools such as Claude Code and Gemini.

## Synopsis

```bash
libra agent status
libra agent list [--json]
libra agent enable [--agent <name>]...
libra agent add [<name>...]
libra agent disable [--agent <name>]...
libra agent remove [<name>...]
libra agent session <subcommand>
libra agent checkpoint <subcommand>
libra agent clean [--all]
libra agent doctor
libra agent push [--remote <name>]
libra agent rpc <subcommand>
```

## Description

`libra agent` manages Libra's external-agent capture surface. It installs and
removes provider hooks, reports captured session/checkpoint state, exposes
read-only diagnostics, and can push `refs/libra/traces` to a remote.

The supported roster is `claude-code`, `codex` and `opencode` (first batch).
`claude-code` is hook-installable today; `codex` and `opencode` are
transcript-readable and gain hook installation once their hook providers land.
`gemini` was demoted out of the supported roster and is uninstall-only:
`libra agent remove gemini` removes previously installed Libra-managed hooks
(idempotent), captured sessions stay readable, and `add`/`enable` for it — or
for any other non-roster agent — return an actionable unsupported error.

## Subcommands

| Subcommand | Description |
|------------|-------------|
| `status` | Report captured external-agent session status |
| `list` | List agents with their capability matrix (roster, hooks, install state) |
| `enable` | Enable one or more external agents and install hooks |
| `add` | Alias of `enable`: `add <name>` ≡ `enable --agent <name>` |
| `disable` | Disable one or more external agents and uninstall hooks |
| `remove` | Alias of `disable`: `remove <name>` ≡ `disable --agent <name>` |
| `session list` | List captured sessions |
| `session show <id>` | Show a captured session |
| `session stop <id>` | Mark a captured session as stopped |
| `session resume <id>` | Mark a stopped captured session active again |
| `session promote <id>` | Promote a captured session into Libra intent metadata |
| `session derive-tool-calls <id>` | Derive tool-call records from a captured session |
| `checkpoint list` | List captured checkpoints |
| `checkpoint show <id>` | Show checkpoint metadata |
| `checkpoint rewind <id>` | Inspect or apply a working-tree rewind for one checkpoint |
| `clean` | Clean up temporary checkpoints from stopped sessions |
| `doctor` | Diagnose hook installation and capture state |
| `push` | Push `refs/libra/traces` to a remote |
| `rpc list` | List discovered `libra-agent-*` binaries on `PATH` (with trusted/quarantined state); requires the external-agents opt-in |
| `rpc trust <slug>` | Trust a discovered binary — records path + sha256 + device/inode/mtime provenance (refused when its directory is world-writable) |
| `rpc untrust <slug>` | Revoke trust; the binary returns to quarantine (always available, even while external agents are disabled) |
| `rpc invoke` | Invoke one JSON-RPC method on a trusted `libra-agent-*` binary |

## Common Options

| Flag | Subcommand | Description |
|------|------------|-------------|
| `--agent <name>` | `enable`, `disable` | Select agent names; omit to target the supported roster (`add`/`remove` take the names positionally) |
| `--extract-transcript <path>` | `session show` | Copy the captured transcript path from session metadata to a local file |
| `--all` | `clean` | Clean all stopped-session checkpoints instead of only the most recent |
| `--remote <name>` | `push` | Select the remote used for pushing agent trace refs |
| `--dry-run` | `checkpoint rewind` | Show the impact without modifying files; this is the default |
| `--apply` | `checkpoint rewind` | Restore the working tree for the selected checkpoint |

## JSON Output

Subcommands that support structured output use the global `--json` and
`--machine` envelope. For example:

```bash
libra --json agent status
libra --json agent list
libra --json agent checkpoint list
libra --json agent rpc list
```

`agent list --json` carries a stable `schema_version` plus one row per known
agent (`slug`, `agent_kind`, `stability`, `supported`, `support_wave`,
`registered`, `transcript_readable`, `hook_installable`, `installed`,
`launchable_review`, `launchable_investigate`, `external_binary`,
`config_paths`, `protected_dirs`, `capabilities`). The row shape is a frozen
contract for automation.

## Examples

```bash
# Show captured-session counts and recent checkpoint summary
libra agent status

# Show the agent capability matrix (supported roster, hooks, install state)
libra agent list

# Enable Claude Code capture and install its hooks (alias of enable)
libra agent add claude-code

# Enable Claude Code capture and install its hooks
libra agent enable --agent claude

# Enable every supported agent at once
libra agent enable

# Disable Claude Code capture and uninstall its hooks (alias of disable)
libra agent remove claude-code

# Remove legacy gemini hooks (uninstall-only channel; idempotent)
libra agent remove gemini

# Disable Claude Code capture and uninstall its hooks
libra agent disable --agent claude

# List captured sessions
libra agent session list

# Show a session and copy its captured transcript
libra agent session show <session-id> --extract-transcript /tmp/session.jsonl

# Stop a captured session
libra agent session stop <session-id>

# Resume a stopped captured session
libra agent session resume <session-id>

# List captured checkpoints
libra agent checkpoint list

# Show a single checkpoint by id
libra agent checkpoint show <id>

# Replay a checkpoint as a JSONL transcript
libra agent checkpoint rewind <id>

# Drop temporary checkpoints from the most recent stopped session
libra agent clean

# Drop temporary checkpoints from every stopped session
libra agent clean --all

# Diagnose hook installation and capture state
libra agent doctor

# Push refs/libra/traces to the default remote
libra agent push

# Push refs/libra/traces to a named remote
libra agent push --remote origin

# Discover libra-agent-<name> RPC binaries on PATH
libra agent rpc list

# Invoke a single JSON-RPC method on a libra-agent-<slug> binary
libra agent rpc invoke <slug> <method> --params '<json>'

# Structured JSON envelope for agents
libra agent --json status
```

The same banner is rendered by `libra agent --help` so the doc and the
CLI surface stay in sync (cross-cutting `--help` EXAMPLES rollout, see
`docs/development/commands/_general.md` item B).

## Notes

- External `libra-agent-*` agents are **disabled by default**. Opt in with
  `libra config set agent.external_agents.enabled true` (repo-local); until
  then `rpc list`/`rpc trust`/`rpc invoke` refuse with `LBR-AGENT-002`
  (`rpc untrust` stays available — revoking trust only tightens security).
  Discovered binaries stay quarantined until `rpc trust <slug>` records
  their provenance (trust is refused for a binary in a world-writable
  directory), every invoke revalidates it (drift revokes trust,
  `LBR-AGENT-005`), the child environment is cleared to an allowlist, and
  stderr is captured/capped/redacted — never inherited. Invoke timeouts,
  broken pipes and malformed frames map to `LBR-AGENT-012`; IO hard-cap
  violations map to `LBR-AGENT-007`.


- The top-level `agent hooks` entry is hidden and intended for hook configs
  installed by `libra agent enable`; users normally do not call it directly.
- `checkpoint rewind --apply` restores working-tree files only; the agent's own
  transcript file is not rewritten.
- Hook and capture diagnostics are best-effort and are designed to report
  actionable installation state rather than silently ignoring missing providers.
