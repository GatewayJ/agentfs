# 命令行使用

[English](cli.md) | [简体中文](cli.zh-CN.md)

## 启动守护进程

按照 [README](../README.zh-CN.md) 在仓库根目录构建。以下示例使用 POSIX shell 和 `jq` 构造 JSON、读取返回的 ID。请在每个终端中将构建后的程序加入 `PATH`：

```sh
export PATH="$PWD/target/debug:$PATH"
export AGENTFS_DIRECTORY="$HOME/.local/share/agentfs"
agentfsd init --directory "$AGENTFS_DIRECTORY"
agentfsd serve --config "$AGENTFS_DIRECTORY/agentfs.json"
```

新目录只需运行一次 `init`。该命令会输出生成的配置路径，已有配置或 token 时会拒绝执行。`serve` 在前台运行，Ctrl+C 会请求关闭服务。使用[远端配置](configuration.zh-CN.md)时，请在当前终端运行 `serve` 前设置 S3 凭据。

| 命令 | 参数 |
| --- | --- |
| `agentfsd init` | 必填 `--directory <PATH>`；可选 `--listen <IP:PORT>`，默认 `127.0.0.1:7421`。 |
| `agentfsd serve` | 必填 `--config <PATH>`。 |
| `agentfs tools` | 以 JSON 输出工具目录，包括 `inputSchema` 和 annotations。 |
| `agentfs call <TOOL>` | 可选 `--json <JSON>`，默认 `{}`；可选 `--request-id <ID>`。 |
| `agentfs mcp` | 启动 stdio MCP 代理，连接已有守护进程。 |

两个程序都提供 `--help` 和 `--version`，子命令提供 `--help`。

## 客户端连接与输出

连接参数放在**子命令前面**：

```sh
agentfs --endpoint http://127.0.0.1:7421/mcp --token-file "$AGENTFS_DIRECTORY/token" tools
```

| 参数 / 环境变量 | 行为 |
| --- | --- |
| `--endpoint` / `AGENTFS_ENDPOINT` | 包含 `/mcp` 的完整 MCP URL，默认 `http://127.0.0.1:7421/mcp`。命令行参数覆盖环境变量。 |
| `--token-file` / `AGENTFS_TOKEN_FILE` | 读取 token 文件，并去除首尾空白。命令行参数覆盖环境变量。 |
| `AGENTFS_TOKEN` | 未选择 token 文件时使用。已选择的 token 文件无法读取时，会返回错误。 |
| `RUST_LOG` | 日志过滤设置，客户端默认 `warn`。日志写入 stderr。 |

守护进程需要已经启动。工具参数中的主机路径，例如导入来源和挂载路径，属于守护进程所在机器。`--token-file` 和标准输入属于客户端所在机器。

`tools` 输出包含 `tools` 数组的 MCP 工具目录对象。`call` 以 JSON 输出工具的 `structuredContent`。错误包含 `error.code`、`error.message` 和 `error.retryable`；操作结果记录可能同时包含已保存的结果和错误。对于操作，请分别检查 `phase`、`local_saved`、`remote_confirmed` 和 `mount_ready`。

工具响应没有错误时，客户端退出码为 0；工具返回错误时为 2；传输、身份验证、token 读取或 JSON 输入发生错误时为 1。命令行参数解析错误也使用退出码 2。`waiting_for_quiesce` 可能伴随退出码 0，请检查操作结果后再继续依赖该操作的任务。

`--json -` 从标准输入读取内容，上限为 8 MiB。HTTP 请求正文同样限制为 8 MiB，该限制包含 MCP 外层结构。`--request-id` 会将输入 JSON 包装为 `{ "request_id": "...", "request": { ... } }`。请将它用于 [MCP 参考](mcp.zh-CN.md)列出的命令工具。查询工具和 `operation_cancel` 直接接收参数。重试时保留相同的 request ID 和参数，新操作需要使用新的 request ID。

## 创建、导入、读取和导出

在另一个终端设置相同的 `PATH` 和 `AGENTFS_DIRECTORY`。独立运行另一组示例时，请使用新的 `RUN_ID`、来源路径和导出路径。该示例可以在 macOS 默认构建上运行，无需原生挂载。

```sh
export AGENTFS_TOKEN_FILE="$AGENTFS_DIRECTORY/token"
RUN_ID=readme-demo-1
agentfs tools > "$AGENTFS_DIRECTORY/tools.json"
agentfs call workspace_create --request-id "$RUN_ID-create" --json '{"name":"demo"}' > "$AGENTFS_DIRECTORY/demo-create.json"
WORKSPACE="$(jq -r '.result.workspace.id' "$AGENTFS_DIRECTORY/demo-create.json")"

jq -n --arg workspace "$WORKSPACE" '{workspace:$workspace}' |
  agentfs call workspace_status --json - > "$AGENTFS_DIRECTORY/demo-status.json"

mkdir -p "$AGENTFS_DIRECTORY/exchange/source"
printf 'Hello from AgentFS\n' > "$AGENTFS_DIRECTORY/exchange/source/hello.txt"
jq -n --arg workspace "$WORKSPACE" --arg source "$AGENTFS_DIRECTORY/exchange/source" '{workspace:$workspace,source:$source}' |
  agentfs call workspace_import --request-id "$RUN_ID-import" --json - > "$AGENTFS_DIRECTORY/demo-import.json"
REVISION="$(jq -r '.result.branch.formal_head' "$AGENTFS_DIRECTORY/demo-import.json")"
BRANCH="$(jq -r '.result.branch.id' "$AGENTFS_DIRECTORY/demo-import.json")"

jq -n --arg workspace "$WORKSPACE" --arg revision "$REVISION" '{workspace:$workspace,revision:$revision,path:"/hello.txt",offset:0,size:4096}' |
  agentfs call revision_read --json -
jq -n --arg workspace "$WORKSPACE" --arg branch "$BRANCH" '{workspace:$workspace,branch:$branch}' |
  agentfs call revision_list --json -

jq -n --arg workspace "$WORKSPACE" --arg revision "$REVISION" --arg destination "$AGENTFS_DIRECTORY/exchange/export" '{workspace:$workspace,revision:$revision,destination:$destination}' |
  agentfs call workspace_export --request-id "$RUN_ID-export" --json -
```

创建操作返回 `result.workspace`。导入会创建新分支并返回 `result.branch`，其中的 `formal_head` 是导入后的版本。受保护的 `main` 分支接受经过验证的合并。`revision_read` 返回 `{ "bytes": [0, 1, ...], "length": ... }`，文件内容表示为整数字节数组，单次请求的大小最多为 4 MiB。导出要求父目录已经存在，目标目录尚未存在。

## 会话与轮次

在同一终端继续执行，使用前述示例获得的工作区、版本和状态：

```sh
LOCATION="$(jq -r '.location' "$AGENTFS_DIRECTORY/demo-status.json")"
SESSION_KEY="$(jq -n --arg workspace "$WORKSPACE" --arg location "$LOCATION" '{workspace:$workspace,app_namespace:"example",session_id:"demo-session",location:$location}')"
jq -n --argjson key "$SESSION_KEY" --arg source_revision "$REVISION" '{key:$key,source_revision:$source_revision}' |
  agentfs call session_open --request-id "$RUN_ID-open" --json -
jq -n --argjson key "$SESSION_KEY" '{key:$key,turn_id:"turn-1"}' |
  agentfs call turn_begin --request-id "$RUN_ID-begin" --json -
jq -n --argjson key "$SESSION_KEY" '{key:$key,turn_id:"turn-1",status:"completed"}' |
  agentfs call turn_end --request-id "$RUN_ID-end" --json -
jq -n --argjson key "$SESSION_KEY" '{key:$key}' |
  agentfs call session_close --request-id "$RUN_ID-close" --json -
```

该示例创建并关闭一个没有挂载的会话。需要为 Agent 提供原生目录时，请启用平台驱动，并在 `session_open` 中提供位于配置 `mount_roots` 下的绝对路径 `mount_path`。等到 `mount_ready: true` 后启动 Agent。另一个会话需要使用新的 session ID。已暂停或中断的会话使用 `session_resume`；挂载释放和恢复说明见[操作文档](operations.zh-CN.md)。

## 从文件读取请求

JSON 文件保存工具的请求字段。例如，`create-request.json` 可以包含 `{ "name": "another-workspace" }`：

```sh
agentfs call workspace_create --request-id create-from-file --json - < create-request.json
```

PowerShell 示例：

```powershell
$env:AGENTFS_TOKEN_FILE = 'C:\Users\alice\agentfs\token'
Get-Content -Raw .\create-request.json | .\target\debug\agentfs.exe call workspace_create --request-id create-from-file --json -
```

推送和拉取示例见 [S3 配置](configuration.zh-CN.md)；guard、缓存范围、合并和操作结果恢复参数见 [MCP 参考](mcp.zh-CN.md)。
