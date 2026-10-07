# 验证

[English](validation.md) | [简体中文](validation.zh-CN.md)

通过多核心 Make 执行标准检查：

```sh
make -j "$(getconf _NPROCESSORS_ONLN)" verify
make -j "$(getconf _NPROCESSORS_ONLN)" build
python3 tests/cli_smoke.py
```

流程测试覆盖并发请求重试、原子追加、持有打开句柄时重命名、重启恢复、不可变轮次结果、自动保存时间、远端失败和确认丢失、所有权交接、合并验证、过期挂载变更、缓存保留与回收，以及离线预取。持久化测试覆盖摘要失败、读取租约、事务回滚、请求恢复和未知格式拒绝。MCP 测试验证并发客户端、principal 隔离、身份验证，以及 Host/Origin 策略。CLI 测试启动实际进程，验证 stdio MCP、导入和导出的内容与元数据、允许的路径，以及进程重启。

## 原生文件系统

安装并启用平台驱动后执行：

```sh
make -j "$(getconf _NPROCESSORS_ONLN)" test CARGO_FLAGS='-p agentfs-platform --test native_mount -- --ignored --nocapture'
```

macOS 在 `CARGO_FLAGS` 中添加 `--features macos-mount`。Windows 在 PowerShell 中使用 `make -j $env:NUMBER_OF_PROCESSORS`。集成测试管理自己的临时状态和挂载目录，覆盖大于 3 MiB 的文件、截断、随机位置写入、原子追加、重命名、fsync、超过 128 个目录项、历史版本只读访问，以及卸载。

WinFsp wrapper 的空指针卷 flush 回归测试在 Windows 上单独执行：

```powershell
make -j $env:NUMBER_OF_PROCESSORS test CARGO_FLAGS='--manifest-path vendor/winfsp_wrs/Cargo.toml --lib'
```

## 容器验证

准备 Docker 和 `debian:bookworm-slim` 后执行：

```sh
make -j "$(getconf _NPROCESSORS_ONLN)" test CARGO_FLAGS='-p agentfs-platform --test validation -- --ignored --nocapture'
```

测试以无特权容器用户运行，读取具有私有权限的候选内容，验证输入挂载为只读，并检查输出和时间限制。生成候选目录时会授予验证用户读取权限，同时保留可执行文件行为；容器挂载权限防止内容修改。

## S3 / RustFS

使用可清理的独立 endpoint，以及允许创建 `agentfs-integration` bucket 的凭据。测试对象使用唯一前缀。请勿将这些测试连接到生产存储。

```sh
export AGENTFS_TEST_S3_ENDPOINT=http://127.0.0.1:9000
export AWS_ACCESS_KEY_ID=AGENTFSTESTLOCAL
export AWS_SECRET_ACCESS_KEY=agentfs-local-integration-secret-2026
make -j "$(getconf _NPROCESSORS_ONLN)" test CARGO_FLAGS='-p agentfs-s3 -- --ignored --nocapture'
python3 tests/replica_smoke.py
```

后端测试覆盖竞争条件下的引用创建、过期条件更新、空文件、摘要拒绝，以及 20 MiB 对象的分段发布。两个守护进程的测试会推送导入的版本，在另一个 location 拉取并导出该版本，并通过实际 S3 后端交接分支所有权。

## 执行记录

| 检查 | 已验证环境 |
| --- | --- |
| Workspace 测试和 Clippy | macOS、Linux、Windows |
| 原生挂载文件操作和版本访问 | Linux root 用户与普通用户；安装 WinFsp 2.1 的 Windows |
| CLI、stdio MCP、目录元数据、重启恢复 | macOS、Linux、Windows |
| S3 条件写入和分段内容 | 隔离的 RustFS 实例 |
| 两个守护进程之间的 S3 同步和所有权交接 | macOS 和 Linux |
| 容器验证权限、输出限制、超时 | macOS 上的 Docker |
| macFUSE 回调编译 | macOS |

[GitHub Actions](https://github.com/GatewayJ/agentfs/actions) 会为每次提交运行仓库内的测试矩阵，其中包括 Linux 容器验证。macOS 原生挂载需要安装并启用 macFUSE 驱动，该内核挂载测试尚未在开发环境执行。

[CLI 使用](cli.zh-CN.md)中的创建、导入、读取、导出和会话示例已通过临时守护进程执行验证。[MCP 参数表](mcp.zh-CN.md)已与运行中的守护进程返回的 33 个工具 schema 核对。S3 配置已根据守护进程配置加载器、适配器，以及锁定版本 `object_store` 的凭据和 endpoint 选择逻辑检查。上述文档检查不代表已经在 AWS S3 部署环境完成验证。
