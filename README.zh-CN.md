# AgentFS

[English](README.md) | [简体中文](README.zh-CN.md)

AgentFS 为 Agent 提供可持久保存的工作区、隔离的会话分支、不可变的版本记录，以及原生文件系统路径。长期运行的 Rust 守护进程负责管理本地状态和挂载；多个 MCP 客户端和 CLI 可以并发访问同一套经过身份验证的 API。

功能包括：

- 跨平台文件名称、按内容寻址的对象、分页的目录和 inode 索引，以及 SQLite 事务。
- Linux FUSE、macOS macFUSE 和 Windows WinFsp 适配；快照、分支恢复、会话和轮次历史。
- S3/RustFS 同步、限制内存用量的流式传输、离线缓存保留、在线所有权交接，以及经过验证的三方合并候选结果。
- 通过需要身份验证的 Streamable HTTP 提供 33 个具有明确类型的 MCP 工具，同时提供 stdio 代理和 JSON CLI。

## 构建

需要 Rust stable（声明的最低版本为 1.90）、用于编译内置 SQLite 的 C 编译器，以及 GNU Make。构建任务通过 Make 的 jobserver 管理并行度。

```sh
make -j "$(getconf _NPROCESSORS_ONLN)" build
make -j "$(getconf _NPROCESSORS_ONLN)" verify
```

| 平台 | 原生文件系统要求 | 构建方式 |
| --- | --- | --- |
| Linux | FUSE 3 工具、`/dev/fuse` 和挂载权限 | 使用默认 features |
| macOS | 已安装并启用 [macFUSE](https://macfuse.github.io/)，以及 `pkg-config` | `make -j "$(sysctl -n hw.ncpu)" build FEATURES=macos-mount` |
| Windows | MSVC，以及包含 Developer 功能的 [WinFsp 2.1](https://github.com/winfsp/winfsp/releases/tag/v2.1) | PowerShell：`make -j $env:NUMBER_OF_PROCESSORS build` |

macOS 默认构建包含守护进程、CLI 和存储操作。原生挂载需要启用 `macos-mount`。Windows 构建会从注册的安装路径加载 WinFsp；Windows 状态目录应位于本地 NTFS 卷。

## 运行

```sh
AGENTFS_DIRECTORY="$HOME/.local/share/agentfs"
./target/debug/agentfsd init --directory "$AGENTFS_DIRECTORY"
./target/debug/agentfsd serve --config "$AGENTFS_DIRECTORY/agentfs.json"
```

在另一个终端执行：

```sh
export AGENTFS_TOKEN_FILE="$HOME/.local/share/agentfs/token"
./target/debug/agentfs tools
./target/debug/agentfs call workspace_create --request-id create-demo --json '{"name":"demo"}'
./target/debug/agentfs call workspace_list
```

`init` 会创建配置、随机 bearer token，以及状态、挂载和文件交换所需的目录。如果配置已经存在，初始化会拒绝执行，已有凭据会保留。守护进程默认监听 `127.0.0.1:7421`，MCP endpoint 为 `/mcp`。每个状态目录只能由一个守护进程使用。

通过 stdio 连接 MCP 客户端时，将可执行程序设置为 `agentfs`，参数设置为 `["mcp"]`，并设置 `AGENTFS_TOKEN_FILE`。可以通过 `AGENTFS_ENDPOINT` 指定其他守护进程 endpoint。stdio 进程会将请求转发给正在运行的守护进程。

使用 `agentfs tools` 查看 JSON 输入 schema。通过 MCP 调用命令工具时，请求格式为 `{ "request_id": "stable-id", "request": { ... } }`；CLI 的 `--request-id` 选项会构造该请求结构。查询工具和 `operation_cancel` 直接接收参数。`--json -` 从标准输入读取 JSON。工具响应没有错误时，CLI 返回退出码 0；工具返回错误时返回 2；传输或输入发生错误时返回 1；命令行参数解析错误也使用 2。可执行示例见 [CLI 使用](docs/cli.zh-CN.md)，完整工具参考见 [MCP 参数与能力](docs/mcp.zh-CN.md)。

## 使用会话

1. 创建或导入工作区。`workspace_status` 会返回 `location`、分支 ID、所有权 epoch 和 generation。
2. 调用 `session_open`，传入 `key = {workspace, app_namespace, session_id, location}`，以及配置允许的挂载根目录下的绝对路径 `mount_path`。操作会返回会话分支和挂载状态。
3. 使用会话 key 和稳定的 `turn_id` 调用 `turn_begin`，然后让 Agent 在挂载目录中执行任务。
4. 调用 `turn_end`，状态设置为 `completed`、`failed` 或 `cancelled`。内容发生修改时会生成新版本；内容没有变化时保留已有版本，并记录本轮结果。
5. 使用 `session_pause`、`session_resume` 和 `session_close` 管理会话与挂载的关联。需要远端确认时，请求 `durability: "remote"`。

需要并发保护的变更会检查分支 owner、epoch、generation 和 head。这些值应从 `workspace_status` 获取，请勿自行编造。需要停止当前挂载的操作会返回 `waiting_for_quiesce`。请停止使用该目录的进程，关闭相关文件句柄，并使进程退出该目录，然后携带 operation ID 和预期的 binding generation 调用 `mount_release`。详见[操作与恢复](docs/operations.zh-CN.md)。

## 存储与配置

`agentfs.json` 使用绝对文件系统路径。`mount_roots` 指定允许的挂载目标；`directory_roots` 指定允许的导入和导出路径。导入期间源目录需要保持稳定，导入操作会拒绝 symbolic link 和不兼容的文件名称。导出操作会创建新目录，并保留文件内容、修改时间和支持的权限。

要启用远端同步，请在生成的配置中设置 `remote`：

```json
{
  "bucket": "agentfs",
  "prefix": "agentfs/v1",
  "region": "us-east-1",
  "endpoint": "https://s3.example.com",
  "allow_http": false
}
```

通过 `object_store` 支持的 AWS 环境变量或凭据提供程序配置凭据。bucket 必须已经存在，并支持条件对象写入、带条件的分段上传完成操作，以及强一致读取。凭据权限应限制在选定的 bucket/prefix 内。本地开发 endpoint 可以明确启用 `allow_http`。

全部配置字段、AWS S3/RustFS 示例、支持的凭据来源、环境变量优先级和多台机器配置见[配置与 S3](docs/configuration.zh-CN.md)。

缓存默认上限为 2 GiB，回收目标为 1.5 GiB。当前分支根对象、等待发布的数据、合并候选结果和明确指定保留的缓存会受到保护。已经获得远端确认的历史文件内容可以从本地缓存清除。`cache_status.offline_ready` 要求指定保留的数据范围已完整缓存在本地。受保护的数据可能超过缓存目标，工作区容量限制仍然适用。

## 验证与功能范围

CI 会在 Linux、macOS 和 Windows 上运行 workspace 测试，并执行 Linux/Windows 原生挂载测试、macFUSE 回调编译检查、CLI/stdio 集成测试，以及连接实际 RustFS 服务的后端测试。原生挂载测试覆盖数 MiB 的文件写入、截断、追加、持有打开句柄时重命名、目录枚举、fsync、历史版本只读挂载和卸载。macOS 内核挂载需要已启用的 macFUSE 安装环境，并通过同一项测试手动验证。测试命令和当前验证记录见[验证文档](docs/validation.zh-CN.md)。

文件操作支持普通文件和目录、分支内原子追加、重命名、权限、时间戳、截断和 fsync。文件内容以整个文件为单位保存为不可变对象；编辑后的文件通过流式处理重新生成对象。分支和合并操作会在内存中构建元数据树。每个工作区可配置的 inode 数量上限为一百万；容量限制应根据可用内存和磁盘空间设置。

能力查询响应会说明支持的行为。当前接口不支持 symbolic link、hard link、扩展属性、内存映射写入、advisory locking、自动文本冲突解决、远端对象物理回收，以及全局会话位置仲裁。删除已打开的文件，或替换已打开的目标文件，会返回 busy 错误。同一路径的并发变更会产生明确的合并冲突。挂载使用守护进程的操作系统身份，工作区访问授权控制 MCP 请求。

更多说明见[文档目录](docs/README.zh-CN.md)、[架构](docs/architecture.zh-CN.md)、[操作与恢复](docs/operations.zh-CN.md)和[验证](docs/validation.zh-CN.md)。AgentFS 使用 Apache-2.0 许可证；仓库内附带的 WinFsp Rust wrapper 保留其 MIT 许可证。
