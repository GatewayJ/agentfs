# AgentFS

AgentFS gives agents durable workspaces, isolated session branches, immutable revisions, and native filesystem paths. A long-running Rust daemon owns local state and mounts. Concurrent MCP clients and the CLI share its authenticated API.

The implementation includes:

- Portable file names, content-addressed objects, paged directory/inode indexes, and SQLite transactions.
- Linux FUSE, macOS macFUSE, and Windows WinFsp adapters; snapshots, branch restore, session and turn history.
- S3/RustFS replication, bounded streaming transfers, offline cache pins, online ownership transfer, and validated three-way merge candidates.
- 33 typed MCP tools over authenticated Streamable HTTP, with a stdio proxy and JSON CLI.

## Build

Use Rust stable (minimum declared version 1.90), a C compiler for bundled SQLite, and GNU Make. Build jobs use Make's jobserver.

```sh
make -j "$(getconf _NPROCESSORS_ONLN)" build
make -j "$(getconf _NPROCESSORS_ONLN)" verify
```

| Platform | Native filesystem requirements | Build |
| --- | --- | --- |
| Linux | FUSE 3 utilities, `/dev/fuse`, permission to mount | Default features |
| macOS | [macFUSE](https://macfuse.github.io/) installed and enabled, `pkg-config` | `make -j "$(sysctl -n hw.ncpu)" build FEATURES=macos-mount` |
| Windows | MSVC, [WinFsp 2.1](https://github.com/winfsp/winfsp/releases/tag/v2.1) including its Developer feature | PowerShell: `make -j $env:NUMBER_OF_PROCESSORS build` |

The default macOS build provides the daemon, CLI, and storage operations. Enable `macos-mount` for native mounts. The Windows build loads WinFsp from its registered installation path. Use a local NTFS volume for Windows state.

## Run

```sh
AGENTFS_DIRECTORY="$HOME/.local/share/agentfs"
./target/debug/agentfsd init --directory "$AGENTFS_DIRECTORY"
./target/debug/agentfsd serve --config "$AGENTFS_DIRECTORY/agentfs.json"
```

In another terminal:

```sh
export AGENTFS_TOKEN_FILE="$HOME/.local/share/agentfs/token"
./target/debug/agentfs tools
./target/debug/agentfs call workspace_create --request-id create-demo --json '{"name":"demo"}'
./target/debug/agentfs call workspace_list
```

`init` creates configuration, a random bearer token, and directories for state, mounts, and file exchange. Existing credentials are preserved by refusing initialization over an existing configuration. The daemon defaults to `127.0.0.1:7421`; its MCP endpoint is `/mcp`. Run one daemon per state directory.

To connect an MCP client through stdio, configure the executable as `agentfs`, arguments as `["mcp"]`, and set `AGENTFS_TOKEN_FILE`. `AGENTFS_ENDPOINT` selects another daemon endpoint. The stdio process forwards requests to the running daemon.

Use `agentfs tools` as the source for JSON input schemas. Mutation tools accept `{ "request_id": "stable-id", "request": { ... } }` over MCP. The CLI's `--request-id` option builds that envelope. Query tools receive their arguments directly. `--json -` reads JSON from standard input. The CLI exits with 0 for successful tools, 2 for a tool error, and 1 for transport or input errors.

## Work with sessions

1. Create or import a workspace. `workspace_status` returns its `location`, branch IDs, ownership epochs, and generations.
2. Call `session_open` with `key = {workspace, app_namespace, session_id, location}` and an absolute `mount_path` under a configured mount root. The operation returns the session branch and mount state.
3. Call `turn_begin` with the session key and a stable `turn_id`. Run the agent against the mounted directory.
4. Call `turn_end` with `completed`, `failed`, or `cancelled`. Modified content produces a revision; an unchanged turn retains the existing revision and still records its outcome.
5. Use `session_pause`, `session_resume`, and `session_close` to manage the attachment. Request `durability: "remote"` when remote confirmation is required.

The branch owner, epoch, generation, and head are checked for guarded changes. Obtain current values from `workspace_status`; do not manufacture them. Operations that need an active mount to stop return `waiting_for_quiesce`. Stop processes using that directory, close their handles and current working directories, then call `mount_release` with the operation ID and expected binding generation. See [operations and recovery](docs/operations.md).

## Storage and configuration

`agentfs.json` uses absolute filesystem paths. `mount_roots` permits mount destinations; `directory_roots` permits import and export paths. Imports require a stable source directory and reject symbolic links and incompatible names. Exports create a new directory and preserve file bytes, modification times, and supported permissions.

To enable replication, set `remote` in the generated configuration:

```json
{
  "bucket": "agentfs",
  "prefix": "agentfs/v1",
  "region": "us-east-1",
  "endpoint": "https://s3.example.com",
  "allow_http": false
}
```

Supply credentials using the AWS environment/provider configuration supported by `object_store`. The bucket must already exist and support conditional object writes, conditional multipart completion, and strongly consistent reads. Restrict credentials to the selected bucket/prefix. Local development endpoints may explicitly enable `allow_http`.

The cache defaults to a 2 GiB limit and a 1.5 GiB collection target. Current branch roots, pending publications, merge candidates, and explicit pins remain protected. Historical file content confirmed remotely can be evicted. `cache_status.offline_ready` requires a complete pinned selection. Protected data can exceed the cache target; the workspace capacity limits still apply.

## Validation and scope

CI runs workspace tests on Linux, macOS, and Windows; native mount tests on Linux and Windows; macFUSE callback compilation; CLI/stdio integration; and a real RustFS backend test. The native mount test covers multi-megabyte writes, truncation, append, rename with an open handle, directory enumeration, fsync, historical read-only mounts, and unmount. macOS kernel mounting requires an enabled macFUSE installation and is verified manually with the same test. See [validation](docs/validation.md) for commands and current execution evidence.

File operations cover regular files and directories, atomic branch append, rename, permissions, timestamps, truncate, and fsync. File contents are stored as whole-file immutable objects; edited files are resealed by streaming. Metadata trees are materialized in memory for branch and merge operations. The configurable maximum is one million inodes per workspace; size limits should reflect available memory and disk.

The advertised capability response describes the supported behavior. Symbolic links, hard links, extended attributes, memory-mapped writes, advisory locking, automatic text conflict resolution, physical remote garbage collection, and global session-location arbitration are outside the implemented interface. Unlinking an open file or replacing an open destination returns a busy error. Concurrent changes to the same path become explicit merge conflicts. Mounts are owned by the daemon's OS identity; workspace access grants govern MCP requests.

See [architecture](docs/architecture.md), [operations and recovery](docs/operations.md), and [validation](docs/validation.md). AgentFS is licensed under Apache-2.0; the vendored WinFsp Rust wrapper retains its MIT license.
