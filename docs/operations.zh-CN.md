# 操作与恢复

[English](operations.md) | [简体中文](operations.zh-CN.md)

## 请求身份与结果记录

对于命令工具，相同工具和相同参数的重试应复用 `request_id`。请求身份属于当前通过身份验证的 principal，并在重启后保留。冲突复用会返回 `REQUEST_ID_CONFLICT`。命令会在执行记录中的流程前登记操作，`operation_by_request` 和 `operation_get` 可以恢复原始结果记录。`operation_cancel` 直接接收已有 operation ID，详见 [MCP 参数参考](mcp.zh-CN.md)。

请分别检查以下结果字段：

| 字段 | 含义 |
| --- | --- |
| `local_saved` | 操作结果及其引用的本地状态已经提交 |
| `remote_confirmed` | 操作的远端发布已经确认 |
| `mount_ready` | 涉及原生挂载时，请求的挂载关联已经就绪 |
| `phase` | 当前操作阶段 |
| `error` | 具有明确类型的失败信息，可能与已完成的本地保存同时存在 |

远端错误可能伴随 `local_saved: true`。请保留已有 operation ID，恢复远端连接，然后运行 `sync`，或者等待守护进程重试持久化的发布队列。远端超时无法确认条件写入是否已提交，发布程序会检查已记录的操作身份。

## 挂载变更

恢复、应用合并和会话变更可能要求停止使用已挂载的文件系统。处于 `waiting_for_quiesce` 的操作携带原始挂载 generation。请停止 Agent 和其他使用该挂载的进程，关闭文件，并使终端退出该目录。随后携带 operation ID 和 `expected_binding_generation` 调用 `mount_release`。重试 release 操作时，需要复用它自己的稳定 request ID。

修改挂载关联前，操作会再次验证原始 branch guard。目标状态已经变化时，操作会失败，并保留尚未应用的候选结果。generation 检查防止旧句柄继续访问替换后的分支。本地提交成功但挂载失败时，会返回 `committed_mount_pending`，持久化结果保留用于恢复。只有原始挂载关联尚未变化且本地提交尚未完成时，才接受取消操作。

守护进程正常关闭时，会停止 HTTP 流量和后台任务，保存已修改的本地分支，并请求正常卸载。仍在使用中的挂载会报告恢复要求。守护进程或文件系统仍在活动时，请勿删除状态目录。

## 进程重启

启动时会从最后一次已确认的不可变分支根对象恢复工作元数据，并移除可重新生成的工作副本。进程意外终止后，尚未通过 fsync、自动保存、版本操作或轮次保存确认的写入可能丢失。写入失败后，如果元数据持久化也失败，受影响的分支会停止，等待恢复。

中断的会话需要通过 `session_resume` 恢复；存在活动轮次时，需要明确提供 `recovery_action` 和受影响的 `turn_id`。`continue` 继续已记录的轮次，`interrupt` 记录中断后恢复会话。复用原生挂载前会检查其状态。路径上存在无法识别的文件系统时，需要操作人员清理；服务不会强制卸载无关文件系统。

状态目录应位于正确支持文件同步和原子重命名的本地文件系统上。文件系统和存储设备必须遵守同步请求。Unix 发布对象时会同步文件数据和目录；Windows 同步文件数据，并通过 write-through 操作发布文件名称。配置远端存储后，远端确认为数据持久性提供额外保障。

## 同步、缓存与所有权

`sync` 支持 `push`、`pull` 和 `both`。在新的 location 拉取工作区会导入其历史，之后可以使用导入的版本创建分支和打开会话。持有远端历史引用不会获得本地写入所有权。

使用 `cache_prefetch` 下载指定版本或路径。使用 `cache_pin` 并设置 `enabled: true`，保留完整选择范围，以供离线使用。`cache_status` 会报告内容是否完整以及是否受到保留设置保护。取消保留后，如果不存在其他保护，缓存可以在之后回收。缓存回收只移除已经获得远端确认、没有活动读取者且没有受保护引用的不可变对象。

所有权交接在线执行。请在旧 location 结束活动轮次，并释放可写挂载。使用当前 `BranchGuard` 和目标 `LocationId` 提交 `ownership_transfer`。旧 location 会停止分支写入，完成发布队列，再更新条件引用。新的 location 通过拉取接管已确认的所有权。远端响应无法确定时，请保留原始交接操作。

## 合并验证

`merge_prepare` 生成候选结果和路径冲突。针对当前候选结果提交 `merge_resolve`，为每个冲突选择 source、target、delete 或替换内容。`merge_validate` 执行结构检查，并且可以运行管理员命名的容器配置。候选结果发生任何变化都会使之前的验证失效。`merge_apply` 需要提供已验证的候选结果和当前 target guard。

`validation` 下的配置示例：

```json
{
  "syntax": {
    "image": "python:3.13-alpine",
    "program": "/usr/local/bin/python",
    "arguments": ["-c", "import ast,pathlib; [ast.parse(p.read_text()) for p in pathlib.Path('/workspace').rglob('*.py')]"],
    "timeout_seconds": 30,
    "max_output_bytes": 65536
  }
}
```

需要预先加载所选镜像，验证使用 `--pull=never`。受控部署应使用不可变的镜像摘要。候选内容以只读方式挂载到 `/workspace`，临时工作空间为 `/tmp`。

远端凭据见[配置与 S3](configuration.zh-CN.md)，可执行示例见 [CLI 使用](cli.zh-CN.md)。
