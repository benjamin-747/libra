# `libra agent`

管理 Claude Code 和 Gemini 等工具的外部代理捕获。

## 概要

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

## 说明

`libra agent` 管理 Libra 的外部代理捕获表面。它安装和移除提供商 hook，报告已捕获的 session/checkpoint 状态，暴露只读诊断，并可将 `refs/libra/traces` 推送到远程。

支持的 roster 为 `claude-code`、`codex`、`opencode`（首批）。`claude-code` 当前可安装 hook；`codex` 与 `opencode` 目前只支持 transcript 读取，其 HookProvider 落地后才可安装。`gemini` 已从支持 roster 降级为仅卸载通道：`libra agent remove gemini` 可移除历史安装的 Libra 托管 hook（幂等），已捕获会话保持可读；对它或其它非 roster 代理执行 `add`/`enable` 会返回可操作的 unsupported 错误。

## 子命令

| 子命令 | 说明 |
|------------|-------------|
| `status` | 报告已捕获的外部代理会话状态 |
| `list` | 列出代理能力矩阵（roster、hook、安装状态） |
| `enable` | 启用一个或多个外部代理并安装 hook |
| `add` | `enable` 的别名：`add <name>` ≡ `enable --agent <name>` |
| `disable` | 禁用一个或多个外部代理并卸载 hook |
| `remove` | `disable` 的别名：`remove <name>` ≡ `disable --agent <name>` |
| `session list` | 列出已捕获会话 |
| `session show <id>` | 显示一个已捕获会话 |
| `session stop <id>` | 将已捕获会话标记为 stopped |
| `session resume <id>` | 将已停止的已捕获会话重新标记为 active |
| `session promote <id>` | 将已捕获会话提升为 Libra intent 元数据 |
| `session derive-tool-calls <id>` | 从已捕获会话推导工具调用记录 |
| `checkpoint list` | 列出已捕获 checkpoint |
| `checkpoint show <id>` | 显示 checkpoint 元数据 |
| `checkpoint rewind <id>` | 检查或应用某个 checkpoint 的工作树回退 |
| `clean` | 清理已停止会话的临时 checkpoint |
| `doctor` | 诊断 hook 安装和捕获状态 |
| `push` | 将 `refs/libra/traces` 推送到远程 |
| `rpc list` | 列出 `PATH` 上发现的 `libra-agent-*` 二进制（含 trusted/quarantined 状态）；需先开启 external-agents 开关 |
| `rpc trust <slug>` | 信任一个已发现的二进制——记录 path + sha256 + device/inode/mtime 来源（所在目录 world-writable 时拒绝） |
| `rpc untrust <slug>` | 撤销信任；二进制回到隔离状态（始终可用，不受开关限制） |
| `rpc invoke` | 在**已信任**的 `libra-agent-*` 二进制上调用一个 JSON-RPC 方法 |

## 常用选项

| 标志 | 子命令 | 说明 |
|------|------------|-------------|
| `--agent <name>` | `enable`, `disable` | 选择代理名称；省略时针对支持 roster（`add`/`remove` 以位置参数接收名称） |
| `--extract-transcript <path>` | `session show` | 将会话元数据中的已捕获 transcript 路径复制到本地文件 |
| `--all` | `clean` | 清理所有已停止会话的 checkpoint，而不只是最近一个 |
| `--remote <name>` | `push` | 选择用于推送代理 trace 引用的远程 |
| `--dry-run` | `checkpoint rewind` | 显示影响而不修改文件；这是默认值 |
| `--apply` | `checkpoint rewind` | 恢复所选 checkpoint 的工作树 |

## JSON 输出

支持结构化输出的子命令使用全局 `--json` 和 `--machine` 信封。例如：

```bash
libra --json agent status
libra --json agent list
libra --json agent checkpoint list
libra --json agent rpc list
```

`agent list --json` 携带稳定的 `schema_version` 与每个已知代理一行（`slug`、`agent_kind`、`stability`、`supported`、`support_wave`、`registered`、`transcript_readable`、`hook_installable`、`installed`、`launchable_review`、`launchable_investigate`、`external_binary`、`config_paths`、`protected_dirs`、`capabilities`）。行结构是面向自动化的冻结契约。

## 示例

```bash
# 显示已捕获会话数量和最近 checkpoint 摘要
libra agent status

# 显示代理能力矩阵（支持 roster、hook、安装状态）
libra agent list

# 启用 Claude Code 捕获并安装它的 hook（enable 的别名）
libra agent add claude-code

# 启用 Claude Code 捕获并安装它的 hook
libra agent enable --agent claude

# 一次启用所有支持的代理
libra agent enable

# 禁用 Claude Code 捕获并卸载它的 hook（disable 的别名）
libra agent remove claude-code

# 移除历史 gemini hook（仅卸载通道；幂等）
libra agent remove gemini

# 禁用 Claude Code 捕获并卸载它的 hook
libra agent disable --agent claude

# 列出已捕获会话
libra agent session list

# 显示一个会话并复制其已捕获 transcript
libra agent session show <session-id> --extract-transcript /tmp/session.jsonl

# 停止一个已捕获会话
libra agent session stop <session-id>

# 继续一个已停止的已捕获会话
libra agent session resume <session-id>

# 列出已捕获 checkpoint
libra agent checkpoint list

# 按 id 显示单个 checkpoint
libra agent checkpoint show <id>

# 将 checkpoint 回放为 JSONL transcript
libra agent checkpoint rewind <id>

# 从最近停止的会话中丢弃临时 checkpoint
libra agent clean

# 从每个已停止会话中丢弃临时 checkpoint
libra agent clean --all

# 诊断 hook 安装和捕获状态
libra agent doctor

# 将 refs/libra/traces 推送到默认远程
libra agent push

# 将 refs/libra/traces 推送到具名远程
libra agent push --remote origin

# 发现 PATH 上的 libra-agent-<name> RPC 二进制文件
libra agent rpc list

# 在 libra-agent-<slug> 二进制文件上调用单个 JSON-RPC 方法
libra agent rpc invoke <slug> <method> --params '<json>'

# 面向代理的结构化 JSON 信封
libra agent --json status
```

`libra agent --help` 会渲染同一横幅，因此文档和 CLI 表面保持同步（跨命令 `--help` EXAMPLES 推出，见 `docs/development/commands/_general.md` 条目 B）。

## 说明

- 外部 `libra-agent-*` 代理**默认禁用**。使用 `libra config set agent.external_agents.enabled true`（仓库级）显式开启；开启前 `rpc list`/`rpc trust`/`rpc invoke` 会以 `LBR-AGENT-002` 拒绝（`rpc untrust` 始终可用——撤销信任只会收紧安全面）。已发现的二进制在 `rpc trust <slug>` 记录来源前保持隔离（world-writable 目录中的二进制拒绝信任）；每次 invoke 都会复验来源（漂移即撤销信任，`LBR-AGENT-005`）；子进程环境被清空为白名单注入，stderr 被捕获/限长/脱敏——绝不继承。invoke 超时、broken pipe、malformed frame 映射 `LBR-AGENT-012`；IO 硬上限超限映射 `LBR-AGENT-007`。

- 顶层 `agent hooks` 入口是隐藏的，面向由 `libra agent enable` 安装的 hook 配置；用户通常不会直接调用它。
- `checkpoint rewind --apply` 只恢复工作树文件；代理自身的 transcript 文件不会被重写。
- Hook 和捕获诊断采用 best-effort 方式，设计目标是报告可操作的安装状态，而不是静默忽略缺失的提供商。
