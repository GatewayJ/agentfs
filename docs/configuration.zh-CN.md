# 配置与 S3

[English](configuration.md) | [简体中文](configuration.zh-CN.md)

## 守护进程配置

运行 `agentfsd init --directory /absolute/path/to/agentfs`，创建 `agentfs.json`、私有 token 文件，以及挂载和文件交换目录。修改生成的配置后，运行 `agentfsd serve --config /absolute/path/to/agentfs/agentfs.json`。配置在启动时读取，修改后需要重新启动守护进程。

| 字段 | 含义与默认值 |
| --- | --- |
| `data_dir` | 必填，本地状态的绝对路径；初始化选择 `<directory>/state`。每个状态目录只能由一个守护进程使用。 |
| `listen` | 必填，本机回环地址和端口；初始化选择 `127.0.0.1:7421`。 |
| `mount_roots` | 必填，允许挂载的绝对根目录数组；初始化创建 `<directory>/mounts`。 |
| `directory_roots` | 允许导入和导出的绝对根目录；省略时为 `[]`。初始化创建并允许使用 `<directory>/exchange`。 |
| `credentials` | 必填，格式为 `{ "principal": "owner", "token_file": "/absolute/path/token" }` 的数组，至少需要一项凭据。 |
| `allowed_origins` | HTTP Origin 允许列表，默认 `[]`。CLI 和 stdio 连接不会发送 Origin。 |
| `cache` | 可选对象，默认 `max_bytes: 2147483648`、`target_bytes: 1610612736`、`min_free_bytes: 67108864`。提供该对象时，需要填写全部三个字段；`target_bytes` 必须小于 `max_bytes`。 |
| `validation` | 具有名称的容器检查配置，默认 `{}`，详见[操作说明](operations.zh-CN.md)。 |
| `docker` | Docker 可执行程序名称或路径，默认 `docker`。 |
| `remote` | 下文的 S3 配置。省略或设置为 `null` 时仅使用本地存储。 |

状态目录、允许的根目录和 token 路径必须使用绝对路径。启动前应创建允许的根目录。JSON 字符串不会展开 `~`、`$HOME` 或环境变量。Windows 路径使用转义后的反斜线，例如 `"C:\\Users\\alice\\agentfs\\state"`，也可以使用正斜线。未知的顶层配置字段和 `remote` 字段会被拒绝。配置文件上限为 1 MiB。

token 文件必须是普通文件，最多 4096 字节，去除首尾空白后至少包含 32 个非空白字节。Unix 环境下，文件不能授予所属组和其他用户访问权限；初始化会创建权限为 `0600` 的文件。各 token 必须唯一。token 选择配置中的 `principal`，工作区所有权和授权决定该身份的访问范围。请使用守护进程用户的文件系统权限保护配置和状态。

## S3 字段

以下对象应配置在 `agentfs.json` 的**顶层 `remote` 字段**中：

```json
{
  "remote": {
    "bucket": "agentfs",
    "prefix": "agentfs/v1",
    "region": "us-east-1",
    "endpoint": "http://127.0.0.1:9000",
    "allow_http": true
  }
}
```

这是使用本地 RustFS S3 API endpoint 的配置片段，请保留其他已生成的字段。`endpoint` 是 S3 API 的基础 URL，不包含 bucket 名称、凭据、查询参数或 fragment。适配器使用 path-style 请求，并追加 bucket 名称。端口应采用部署环境提供的 S3 API 端口。

| `remote` 字段 | 必填要求 / 默认值 | 行为 |
| --- | --- | --- |
| `bucket` | 必填，非空字符串 | 已存在的 bucket 名称。守护进程不会创建 bucket。 |
| `prefix` | `agentfs/v1` | bucket 内共享的命名空间；首尾 `/` 会被移除，结果必须是有效且非空的对象路径。 |
| `region` | `us-east-1` | 签名使用的 region；AWS S3 应填写 bucket 所在的 region。 |
| `endpoint` | 省略或 `null` | 可选的 S3 基础 URL。没有 endpoint 覆盖设置时，依赖库会生成 AWS regional endpoint。 |
| `allow_http` | `false` | 明确使用 HTTP 开发环境 endpoint 时设置为 `true`；HTTPS endpoint 保持 `false`。 |

使用 AWS S3 时，替换 bucket 和 region，并省略自定义 endpoint：

```json
{
  "remote": {
    "bucket": "your-existing-bucket",
    "prefix": "agentfs/v1",
    "region": "eu-west-1",
    "allow_http": false
  }
}
```

## 凭据与环境变量优先级

S3 凭据由**守护进程**读取。请在启动 `agentfsd serve` 的终端或服务环境中设置变量。`AGENTFS_TOKEN_FILE` 用于 CLI 访问 AgentFS 的身份验证。

```sh
export AWS_ACCESS_KEY_ID='your-access-key-id'
export AWS_SECRET_ACCESS_KEY='your-secret-access-key'
# Temporary credentials also require AWS_SESSION_TOKEN.
./target/debug/agentfsd serve --config "$AGENTFS_DIRECTORY/agentfs.json"
```

PowerShell 使用 `$env:AWS_ACCESS_KEY_ID = 'your-access-key-id'` 和 `$env:AWS_SECRET_ACCESS_KEY = 'your-secret-access-key'`，随后启动 `agentfsd.exe`。

当前锁定版本的 `object_store` 按以下顺序选择凭据来源：

1. `AWS_ACCESS_KEY_ID` 和 `AWS_SECRET_ACCESS_KEY`，以及可选的 `AWS_SESSION_TOKEN`。两个 key 字段需要同时提供。
2. `AWS_WEB_IDENTITY_TOKEN_FILE` 和 `AWS_ROLE_ARN`，以及可选的 `AWS_ROLE_SESSION_NAME`。
3. 用于容器任务角色的 `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI`。
4. 用于 EKS Pod Identity 的 `AWS_CONTAINER_CREDENTIALS_FULL_URI` 和 `AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE`。
5. 前述来源均未提供时，使用实例元数据凭据。

当前适配器不会读取 `~/.aws/credentials`、`AWS_PROFILE` 或 AWS CLI SSO 会话。请导出有效凭据，或者使用支持的角色凭据来源。静态环境变量凭据在启动时读取，替换后需要重新启动守护进程。

AgentFS 调用 `AmazonS3Builder::from_env()` 后应用自身配置。配置中的 `bucket`、`region` 和 `allow_http`，包括默认值，都会覆盖对应的 AWS 环境变量。配置中的 `endpoint` 会覆盖 `AWS_ENDPOINT`。依赖库中的 `AWS_ENDPOINT_URL_S3` 优先于这两个设置。通过 `agentfs.json` 选择 endpoint 时，请移除不需要的 `AWS_ENDPOINT_URL_S3`；使用默认 AWS regional endpoint 时，也需要移除 `AWS_ENDPOINT`。适配器会选择 path-style 请求和基于 ETag 的条件写入。

## 存储要求与验证

存储服务需要支持 GET/HEAD、按前缀列举、对象写入、分段上传和终止上传，以及强一致读取。AgentFS 使用 `If-None-Match: *` 创建不可变对象，并以相同条件完成分段上传；更新引用时使用携带之前 ETag 的 `If-Match`。条件写入行为见 [AWS S3 文档](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html)。大于 8 MiB 的文件使用分段上传，分段大小根据文件大小在 8 至 64 MiB 之间选择。存储凭据权限应限制在选定的 bucket 和命名空间内。

守护进程启动时会构建 S3 客户端，但启动成功不能证明 bucket 或凭据可用。按照 [CLI 示例](cli.zh-CN.md)导入内容后，明确执行推送，并查看原始导入操作的结果记录：

```sh
jq -n --arg workspace "$WORKSPACE" '{workspace:$workspace,direction:"push"}' |
  agentfs call sync --request-id "$RUN_ID-push" --json -
jq -n --arg operation "$(jq -r '.id' "$AGENTFS_DIRECTORY/demo-import.json")" '{operation:$operation}' |
  agentfs call operation_get --json -
```

检查原始操作的 `remote_confirmed`。`workspace_status.capabilities.remote_configured` 表示已提供远端配置，`pending_jobs` 表示队列长度。守护进程每三秒重试待同步任务。`local_saved: true` 的结果记录仍然可能包含远端错误；请保留 operation ID，并参考[恢复说明](operations.zh-CN.md)。

要检查条件写入、并发更新、20 MiB 分段内容和两个守护进程之间的同步，请使用隔离的 [S3/RustFS 测试](validation.zh-CN.md)。这些测试使用独立的测试 bucket，不适合作为生产环境健康检查。

## 多台机器

各守护进程需要配置相同的 S3 endpoint、bucket、prefix 和签名 region。每台机器分别初始化状态目录并获得自己的 `LocationId`，然后使用源守护进程返回的 workspace ID。目标机器上通过身份验证的 principal 需要是工作区所有者，或者获得适当授权。各机器的 token 可以不同。

在源机器执行推送，然后在目标机器使用相同 workspace ID 调用 `sync`，设置 `direction: "pull"`。拉取会导入历史和远端分支引用。可以使用已保留的版本创建本地分支或会话；需要接管已有可写分支时，完成在线 `ownership_transfer`。详见[操作说明](operations.zh-CN.md)和 [MCP 参考](mcp.zh-CN.md)。

配置定义见[守护进程](../apps/agentfsd/src/config.rs)、[S3 适配器](../crates/agentfs-s3/src/lib.rs)，依赖版本见 [Cargo.lock](../Cargo.lock)。
