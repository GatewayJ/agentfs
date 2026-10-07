# MCP connection, parameters, and capabilities

[English](mcp.md) | [简体中文](mcp.zh-CN.md)

## Connect a client

Start `agentfsd` using the [CLI guide](cli.md). For clients supporting an `mcpServers` configuration, a stdio connection can use:

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

Replace both paths with actual absolute paths; use `agentfs.exe` on Windows. The exact outer configuration format depends on the client. The proxy uses stdin/stdout for MCP and stderr for logs. It connects to an already running daemon and shares that daemon's state with other clients.

For direct Streamable HTTP, use `http://127.0.0.1:7421/mcp` and `Authorization: Bearer <token-file-contents>`. Use an MCP client that performs initialization and preserves the returned session information. The accepted Host values are the bound address and `localhost:<port>`. A supplied Origin must match `allowed_origins`; native CLI/stdio connections omit Origin. The daemon binds only to loopback; access from other machines requires an authenticated TLS proxy preserving an accepted Host value.

The server advertises the MCP `tools` capability. It provides 33 tools, including their input JSON schemas and annotations. Read the connected server's catalog with `tools/list` or:

```sh
agentfs tools > tools.json
jq '.tools[] | select(.name == "session_open") | .inputSchema' tools.json
```

## Argument forms and results

The 23 command tools in the next table require an envelope. For example, the `arguments` object in a `tools/call` for `workspace_create` is:

```json
{
  "request_id": "create-demo-1",
  "request": {
    "name": "demo"
  }
}
```

The equivalent CLI is `agentfs call workspace_create --request-id create-demo-1 --json '{"name":"demo"}'`. `request_id` must be nonempty, contain no control characters, and be at most 256 UTF-8 bytes. Its identity belongs to the authenticated principal and survives process restarts. Retry with the same ID, tool, and arguments; conflicting reuse returns `REQUEST_ID_CONFLICT`.

The nine query tools receive direct argument objects. `operation_cancel` also receives direct arguments and modifies the referenced operation; it does not accept the command envelope. For `operation_by_request`, use:

```json
{
  "request_id": "create-demo-1"
}
```

Tool argument objects and command request structs reject unknown fields. Consult nested schemas for their accepted properties. Optional nullable fields can be omitted or set to `null`; fields with defaults, such as `durability` and `grants`, can be omitted but do not accept `null`.

Successful data is returned through `structuredContent`. Failures set `isError: true` and include `error: {code, message, retryable}`. Commands return an `OperationRecord`; `operation_get`, `operation_by_request`, and `operation_cancel` also return that record. Inspect `local_saved`, `remote_confirmed`, `mount_ready`, `phase`, `result`, and `error`. `mount_ready` is nullable. A completed local save can coexist with a remote error, and an error-free response can still require mount release. See [operations](operations.md).

## Common parameter types

IDs such as `workspace`, `branch`, `revision`, `merge`, `operation`, and `location` are UUID strings returned by the service. Byte counts and generations are JSON integers. Workspace paths use absolute slash-separated paths such as `/src/main.rs`; host paths refer to the daemon machine and must satisfy its configured roots.

| Type | Fields and behavior |
| --- | --- |
| `BranchGuard` | Required `branch`, `authority_epoch`, `generation`, `expected_head`; optional nullable `mutation_seq`. Obtain the branch from `workspace_status.branches`, map `id` to `branch` and `formal_head` to `expected_head`, and copy the remaining values. Including `mutation_seq` also detects unsaved mutations. Refresh before each new guarded operation. |
| `SessionKey` | Required `workspace`, `app_namespace`, `session_id`, `location`. Use the current daemon's `workspace_status.location`. Namespace and session ID must be nonempty, without control characters, and at most 512 UTF-8 bytes each. |
| `durability` | `local` by default; `remote` requests remote confirmation. Inspect the receipt if remote publication fails. |
| `RevisionRange` | Required `workspace`, `revision`; optional nullable `paths`. Omitted/null selects `/`, including all content. A provided list must be nonempty; directory paths include descendants. |
| `Grant` | Required `principal` and `permissions`; optional nullable `branch`. Permissions are `read`, `write`, `merge`, `manage`; `manage` authorizes every permission. A null branch applies throughout the workspace. |
| `Resolution` | Required `choice` and `path`. `choice` is `source`, `target`, `delete`, or `replace`. `replace` additionally requires `content` as an array of integer bytes and `mode` as an integer, for example `420` for octal `0644`. |

## Command tools

Fields below are inside `request`. A suffix `?` means the field may be omitted. Shared types are defined above. Runtime checks can be stricter than the JSON schema's enum or numeric range.

| Tool | Request fields | Behavior / result |
| --- | --- | --- |
| `workspace_create` | `name`, `grants?`, `max_file_bytes?`, `max_working_bytes?`, `max_inodes?` | Creates a workspace and protected `main`; `result.workspace`. Defaults: 64 GiB/file, 256 GiB working capacity, 1,000,000 inodes. Inodes must be 1–1,000,000; file capacity must be positive and no greater than working capacity; working capacity must be at most `i64::MAX`. |
| `workspace_import` | `workspace`, `source` | Imports a stable allowed host directory into a new branch; `result.branch`. |
| `workspace_export` | `workspace`, `revision`, `destination`, `paths?` | Exports selected paths into a new allowed host directory. Omitted/null paths select the whole revision. |
| `revision_commit` | `guard`, `kind`, `durability?` | `kind` accepts `commit` or `checkpoint`; returns `result.revision` and `result.branch`. |
| `branch_fork` | `workspace`, `source`, `name` | `source` is a retained revision ID; creates an isolated writable branch. |
| `branch_restore` | `guard`, `revision`, `durability?` | Restores a branch, with mount release when required. |
| `session_open` | `key`, `source_revision?`, `source_branch?`, `mount_path?`, `durability?` | Select at most one source; omission selects the workspace's initial revision. `source_branch` captures current working state. Omit `mount_path` for an unmounted session; returns `result.binding`. |
| `session_resume` | `key`, `mount_path?`, `recovery_action?`, `turn_id?`, `durability?` | For recovery with an active turn, supply that `turn_id` and `recovery_action: "continue"` or `"interrupt"`. |
| `session_pause` | `key`, `durability?` | Saves and pauses the session, releasing its mount when required. |
| `session_close` | `key`, `durability?` | Saves and closes the session, releasing its mount when required. |
| `turn_begin` | `key`, `turn_id` | Records the turn before work starts; `result.record`. |
| `turn_end` | `key`, `turn_id`, `status`, `durability?` | `status` accepts `completed`, `failed`, or `cancelled`; saves changed content and an immutable turn result. |
| `sync` | `workspace`, `direction`, `branches?` | `direction`: `push`, `pull`, `both`; omission/null of `branches` selects all. `both` pushes before pulling. On pull, discovery imports remote history before filtering branch IDs in the result. |
| `cache_prefetch` | `workspace`, `revision`, `paths?` | Downloads a `RevisionRange`; `result.status`. |
| `cache_pin` | `range`, `enabled` | `range` is a `RevisionRange`; `true` downloads and protects content, `false` removes that pin. |
| `ownership_transfer` | `guard`, `target` | `target` is the destination `LocationId`; performs an online ownership transfer. |
| `merge_prepare` | `workspace`, `source`, `target`, `base?` | `source` and optional `base` are revision IDs; `target` is a `BranchGuard`; returns `result.candidate`. |
| `merge_resolve` | `merge`, `candidate`, `resolutions` | `candidate` is the current candidate revision ID; creates a new candidate and invalidates prior validation. |
| `merge_validate` | `merge`, `candidate`, `configuration?` | Always checks structure; an optional name selects an administrator-defined container check. Omission/null performs structural validation only. |
| `merge_apply` | `merge`, `candidate`, `target`, `durability?` | Requires successful current validation and a clean target matching its `BranchGuard`; may require mount release. |
| `mount_prepare` | `workspace`, `branch`, `path`, `access`, `revision?`, `durability?` | `access`: `read_write` or `read_only`. A supplied revision requires `read_only`; writable mounts require local ownership and an unprotected branch. |
| `mount_release` | `operation`, `expected_binding_generation` | Continues a pending operation after processes release the original mount. The operation ID comes from the pending receipt. |
| `mount_unmount` | `mount`, `generation` | Uses the binding ID and generation from `workspace_status.mounts`; requests normal unmount. |

## Direct-argument tools

| Tool | Arguments | Result |
| --- | --- | --- |
| `workspace_list` | `{}` | Visible workspace array. |
| `workspace_status` | `workspace` | Workspace, local location, branches, remote references, mounts, pending jobs, capabilities, and storage statistics. |
| `revision_list` | `workspace`, `branch?` | Retained revision array; optional branch filter. |
| `revision_diff` | `workspace`, `before`, `after` | File differences between two revision IDs. |
| `revision_read` | `workspace`, `revision`, `path`, `offset`, `size` | `{bytes, length}`; offset and size are bytes, size at most 4,194,304. |
| `cache_status` | `workspace`, `revision`, `paths?` | `complete`, `offline_ready`, `completed_bytes`, and missing object IDs for the range. `offline_ready` additionally requires pin protection. |
| `merge_get` | `merge` | Current candidate, conflicts, and validation. |
| `operation_get` | `operation` | Operation receipt visible to the principal. |
| `operation_by_request` | `request_id` | Original operation receipt for this principal. |
| `operation_cancel` | `operation` | Cancels an eligible operation before commit or attachment changes; returns its receipt. This tool modifies state. |

## Runtime capabilities

Call `workspace_status` for an existing workspace and inspect `capabilities`:

| Field | Current meaning |
| --- | --- |
| `format_version` | Persisted format version, currently `1`. |
| `platform_mount` | `linux-fuse`, `windows-winfsp`, `macos-macfuse`, or `macos-macfuse-feature-required`, according to the build. This describes the compiled adapter; a successful mount also needs the OS driver and permissions. |
| `file_operations` | `lookup`, `getattr`, `readdir`, `create`, `mkdir`, `read`, `write`, `truncate`, `chmod`, `rename`, `unlink`, `rmdir`, `fsync`. These describe mounted filesystem operations. |
| `remote_configured` | Whether a remote adapter is configured; it does not test connectivity or credentials. |
| `automatic_text_merge` | `false`; conflicts require explicit resolution. |
| `physical_remote_gc` | `false`; remote object deletion is disabled. |
| `single_location_session` | `false`; global coordination does not enforce a single location for each session. Branch ownership still controls writing. |

File editing normally uses the mounted native path; the MCP tools manage workspaces, versions, and lifecycle. The catalog marks the nine queries `readOnlyHint: true` and `destructiveHint: false`; commands and cancellation use the opposite values. All tools currently advertise `idempotentHint: true` and `openWorldHint: true`. These annotations describe tool behavior; authentication, grants, request identities, and state checks enforce access and retries.

Source definitions: [catalog and dispatch](../crates/agentfs-mcp/src/server.rs), [request types](../crates/agentfs-ports/src/api.rs), [shared types](../crates/agentfs-model/src/entities.rs), and [error codes](../crates/agentfs-model/src/error.rs).
