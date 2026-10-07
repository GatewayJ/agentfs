# MCP 连接、参数与能力

[English](mcp.md) | [简体中文](mcp.zh-CN.md)

## 连接客户端

按照 [CLI 指南](cli.zh-CN.md)启动 `agentfsd`。对于支持 `mcpServers` 配置的客户端，可以使用以下 stdio 连接：

```json
{
  "mcpServers": {
    "agentfs": {
      "command": "/absolute/path/to/agentfs",
      "args": ["mcp"],
      "env": {
        "AGENTFS_ENDPOINT": "http://127.0.0.1:7421/mcp",
        "AGENTFS_TOKEN_FILE": "/absolute/path/to/agentfs-data/token"
      }
    }
  }
}
```

请将两个路径替换为实际绝对路径，Windows 使用 `agentfs.exe`。外层配置格式取决于客户端。代理通过 stdin/stdout 传输 MCP，通过 stderr 输出日志。代理连接已经运行的守护进程，并与其他客户端共享该守护进程的状态。

直接连接 Streamable HTTP 时，使用 `http://127.0.0.1:7421/mcp`，并提供 `Authorization: Bearer <token-file-contents>`。MCP 客户端需要完成初始化并保留返回的会话信息。允许的 Host 为监听地址和 `localhost:<port>`。请求携带 Origin 时，需要匹配 `allowed_origins`；原生 CLI/stdio 连接省略 Origin。守护进程仅监听本机回环地址；其他机器访问时，需要使用经过身份验证的 TLS 代理，并保留允许的 Host 值。

服务声明支持 MCP `tools` 能力，提供 33 个工具及其输入 JSON schema 和 annotations。可以通过 `tools/list` 读取已连接服务的工具目录，或者运行：

```sh
agentfs tools > tools.json
jq '.tools[] | select(.name == "session_open") | .inputSchema' tools.json
```

## 参数形式与结果

下表中的 23 个命令工具需要外层请求结构。例如，调用 `workspace_create` 时，`tools/call` 中的 `arguments` 对象为：

```json
{
  "request_id": "create-demo-1",
  "request": {
    "name": "demo"
  }
}
```

对应 CLI 为 `agentfs call workspace_create --request-id create-demo-1 --json '{"name":"demo"}'`。`request_id` 必须非空、不含控制字符，且最多为 256 个 UTF-8 字节。请求身份属于当前通过身份验证的 principal，并在进程重启后保留。重试需要使用相同 ID、工具和参数；冲突复用会返回 `REQUEST_ID_CONFLICT`。

九个查询工具直接接收参数对象。`operation_cancel` 也直接接收参数，并修改指定操作的状态，不接受命令工具的外层结构。调用 `operation_by_request` 时使用：

```json
{
  "request_id": "create-demo-1"
}
```

工具参数对象和命令请求结构会拒绝未知字段。嵌套结构接受的属性请查看对应 schema。可选且允许为空的字段可以省略或设置为 `null`；具有默认值的字段，例如 `durability` 和 `grants`，可以省略，但不接受 `null`。

成功数据通过 `structuredContent` 返回。失败时设置 `isError: true`，并包含 `error: {code, message, retryable}`。命令返回 `OperationRecord`；`operation_get`、`operation_by_request` 和 `operation_cancel` 也返回该记录。请检查 `local_saved`、`remote_confirmed`、`mount_ready`、`phase`、`result` 和 `error`。`mount_ready` 可以为空。本地保存成功时仍然可能发生远端错误，没有错误的响应也可能要求释放挂载。详见[操作说明](operations.zh-CN.md)。

## 公共参数类型

`workspace`、`branch`、`revision`、`merge`、`operation` 和 `location` 等 ID 使用服务返回的 UUID 字符串。字节数和 generation 使用 JSON 整数。工作区路径使用以 `/` 开始的路径，例如 `/src/main.rs`；主机路径属于守护进程所在机器，并且需要符合配置中的目录范围。

| 类型 | 字段与行为 |
| --- | --- |
| `BranchGuard` | 必填 `branch`、`authority_epoch`、`generation`、`expected_head`；可选且允许为空的 `mutation_seq`。从 `workspace_status.branches` 获取分支，将 `id` 用作 `branch`，将 `formal_head` 用作 `expected_head`，并复制其他字段。提供 `mutation_seq` 还可以检查尚未保存的修改。每次新的受保护操作前重新获取当前值。 |
| `SessionKey` | 必填 `workspace`、`app_namespace`、`session_id`、`location`。使用当前守护进程的 `workspace_status.location`。namespace 和 session ID 必须非空、不含控制字符，各自最多为 512 个 UTF-8 字节。 |
| `durability` | 默认 `local`；`remote` 请求远端确认。远端发布失败时检查操作结果记录。 |
| `RevisionRange` | 必填 `workspace`、`revision`；可选且允许为空的 `paths`。省略或设置为 `null` 时选择 `/`，包含全部内容。提供路径列表时不能为空，目录路径包含其后代。 |
| `Grant` | 必填 `principal` 和 `permissions`；可选且允许为空的 `branch`。权限为 `read`、`write`、`merge`、`manage`；`manage` 允许全部权限。branch 为空时，授权适用于整个工作区。 |
| `Resolution` | 必填 `choice` 和 `path`。`choice` 为 `source`、`target`、`delete` 或 `replace`。`replace` 还需要整数字节数组 `content` 和整数 `mode`，例如使用 `420` 表示八进制 `0644`。 |

## 命令工具

下列字段均位于 `request` 中。字段后的 `?` 表示可以省略，公共类型见上文。运行时检查可能进一步限制 JSON schema 中的枚举或数字范围。

| 工具 | 请求字段 | 行为 / 结果 |
| --- | --- | --- |
| `workspace_create` | `name`, `grants?`, `max_file_bytes?`, `max_working_bytes?`, `max_inodes?` | 创建工作区和受保护的 `main`，返回 `result.workspace`。默认单文件 64 GiB、工作容量 256 GiB、1,000,000 个 inode。inode 数量为 1–1,000,000；文件容量必须为正数且不超过工作容量；工作容量最多为 `i64::MAX`。 |
| `workspace_import` | `workspace`, `source` | 将允许范围内保持稳定的主机目录导入新分支，返回 `result.branch`。 |
| `workspace_export` | `workspace`, `revision`, `destination`, `paths?` | 将选定路径导出到允许范围内的新主机目录。省略 paths 或设置为 `null` 时选择整个版本。 |
| `revision_commit` | `guard`, `kind`, `durability?` | `kind` 接受 `commit` 或 `checkpoint`，返回 `result.revision` 和 `result.branch`。 |
| `branch_fork` | `workspace`, `source`, `name` | `source` 是已保留的 revision ID；创建隔离的可写分支。 |
| `branch_restore` | `guard`, `revision`, `durability?` | 恢复分支，需要时通过挂载释放继续操作。 |
| `session_open` | `key`, `source_revision?`, `source_branch?`, `mount_path?`, `durability?` | 最多选择一个来源；省略时选择工作区初始版本。`source_branch` 会保存当前工作状态。省略 `mount_path` 时创建没有挂载的会话，返回 `result.binding`。 |
| `session_resume` | `key`, `mount_path?`, `recovery_action?`, `turn_id?`, `durability?` | 恢复存在活动轮次的会话时，提供该 `turn_id`，并设置 `recovery_action: "continue"` 或 `"interrupt"`。 |
| `session_pause` | `key`, `durability?` | 保存并暂停会话，需要时释放挂载。 |
| `session_close` | `key`, `durability?` | 保存并关闭会话，需要时释放挂载。 |
| `turn_begin` | `key`, `turn_id` | 开始任务前记录轮次，返回 `result.record`。 |
| `turn_end` | `key`, `turn_id`, `status`, `durability?` | `status` 接受 `completed`、`failed` 或 `cancelled`；保存变化的内容和不可变的轮次结果。 |
| `sync` | `workspace`, `direction`, `branches?` | `direction` 为 `push`、`pull`、`both`；省略 branches 或设置为 `null` 时选择全部。`both` 先推送后拉取。拉取时，发现过程会导入远端历史，再过滤结果中的 branch ID。 |
| `cache_prefetch` | `workspace`, `revision`, `paths?` | 下载 `RevisionRange`，返回 `result.status`。 |
| `cache_pin` | `range`, `enabled` | `range` 为 `RevisionRange`；`true` 下载并保护内容，`false` 移除该保留设置。 |
| `ownership_transfer` | `guard`, `target` | `target` 是目标 `LocationId`；执行在线所有权交接。 |
| `merge_prepare` | `workspace`, `source`, `target`, `base?` | `source` 和可选的 `base` 为 revision ID；`target` 为 `BranchGuard`；返回 `result.candidate`。 |
| `merge_resolve` | `merge`, `candidate`, `resolutions` | `candidate` 为当前候选 revision ID；创建新候选结果，并使之前的验证失效。 |
| `merge_validate` | `merge`, `candidate`, `configuration?` | 始终检查结构；可选名称用于选择管理员定义的容器检查。省略或设置为 `null` 时仅执行结构验证。 |
| `merge_apply` | `merge`, `candidate`, `target`, `durability?` | 要求当前候选结果验证成功，目标分支没有未保存的修改并符合 `BranchGuard`；可能要求释放挂载。 |
| `mount_prepare` | `workspace`, `branch`, `path`, `access`, `revision?`, `durability?` | `access` 为 `read_write` 或 `read_only`。提供 revision 时必须选择 `read_only`；可写挂载要求分支由本地拥有且未受保护。 |
| `mount_release` | `operation`, `expected_binding_generation` | 使用目录的进程释放原始挂载后，继续等待中的操作。operation ID 来自等待中的操作结果记录。 |
| `mount_unmount` | `mount`, `generation` | 使用 `workspace_status.mounts` 中的 binding ID 和 generation，请求正常卸载。 |

## 直接接收参数的工具

| 工具 | 参数 | 结果 |
| --- | --- | --- |
| `workspace_list` | `{}` | 当前身份可见的工作区数组。 |
| `workspace_status` | `workspace` | 工作区、本地 location、分支、远端引用、挂载、待处理任务、能力和存储统计。 |
| `revision_list` | `workspace`, `branch?` | 已保留的版本数组，可以按分支过滤。 |
| `revision_diff` | `workspace`, `before`, `after` | 两个 revision ID 之间的文件差异。 |
| `revision_read` | `workspace`, `revision`, `path`, `offset`, `size` | `{bytes, length}`；offset 和 size 以字节计，size 最多为 4,194,304。 |
| `cache_status` | `workspace`, `revision`, `paths?` | 指定范围的 `complete`、`offline_ready`、`completed_bytes` 和缺失的 object ID。`offline_ready` 还要求缓存受到保留设置保护。 |
| `merge_get` | `merge` | 当前候选结果、冲突和验证记录。 |
| `operation_get` | `operation` | 当前 principal 可见的操作结果记录。 |
| `operation_by_request` | `request_id` | 当前 principal 的原始操作结果记录。 |
| `operation_cancel` | `operation` | 在提交或挂载关联发生变化前，取消符合条件的操作并返回其结果记录。该工具会修改状态。 |

## 运行时能力

对已有工作区调用 `workspace_status`，检查 `capabilities`：

| 字段 | 当前含义 |
| --- | --- |
| `format_version` | 持久化格式版本，当前为 `1`。 |
| `platform_mount` | 根据构建结果返回 `linux-fuse`、`windows-winfsp`、`macos-macfuse` 或 `macos-macfuse-feature-required`。该值表示已编译的适配器；挂载成功还需要操作系统驱动和权限。 |
| `file_operations` | `lookup`、`getattr`、`readdir`、`create`、`mkdir`、`read`、`write`、`truncate`、`chmod`、`rename`、`unlink`、`rmdir`、`fsync`，表示挂载文件系统支持的操作。 |
| `remote_configured` | 是否已配置远端适配器，不会测试连接或凭据。 |
| `automatic_text_merge` | `false`，冲突需要明确解决。 |
| `physical_remote_gc` | `false`，远端对象删除功能已禁用。 |
| `single_location_session` | `false`，全局协调不会强制每个会话只能位于一个 location。分支所有权仍然控制写入。 |

文件编辑通常通过挂载后的原生路径完成，MCP 工具负责工作区、版本和生命周期管理。工具目录将九个查询标记为 `readOnlyHint: true`、`destructiveHint: false`；命令和取消操作使用相反的值。全部工具当前声明 `idempotentHint: true` 和 `openWorldHint: true`。这些 annotations 描述工具行为，访问和重试由身份验证、授权、请求身份和状态检查控制。

源代码定义见[工具目录与调用分发](../crates/agentfs-mcp/src/server.rs)、[请求类型](../crates/agentfs-ports/src/api.rs)、[公共类型](../crates/agentfs-model/src/entities.rs)和[错误代码](../crates/agentfs-model/src/error.rs)。
