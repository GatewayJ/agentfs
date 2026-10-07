# Command-line usage

[English](cli.md) | [简体中文](cli.zh-CN.md)

## Start the daemon

Build from the repository root using [README](../README.md). The examples below use POSIX shell and `jq` to construct JSON and read returned IDs. Add the built binaries to `PATH` in each terminal:

```sh
export PATH="$PWD/target/debug:$PATH"
export AGENTFS_DIRECTORY="$HOME/.local/share/agentfs"
agentfsd init --directory "$AGENTFS_DIRECTORY"
agentfsd serve --config "$AGENTFS_DIRECTORY/agentfs.json"
```

Run `init` once for a new directory. It prints the generated configuration path and rejects an existing configuration or token. `serve` runs in the foreground; Ctrl+C requests shutdown. Set S3 credentials in this terminal before `serve` when using a [remote configuration](configuration.md).

| Command | Arguments |
| --- | --- |
| `agentfsd init` | Required `--directory <PATH>`; optional `--listen <IP:PORT>`, default `127.0.0.1:7421`. |
| `agentfsd serve` | Required `--config <PATH>`. |
| `agentfs tools` | Lists the tool catalog, including `inputSchema` and annotations, as JSON. |
| `agentfs call <TOOL>` | Optional `--json <JSON>` (default `{}`) and `--request-id <ID>`. |
| `agentfs mcp` | Runs a stdio MCP proxy connected to the existing daemon. |

Both binaries provide `--help` and `--version`; subcommands provide `--help`.

## Client connection and output

Place connection options **before the subcommand**:

```sh
agentfs --endpoint http://127.0.0.1:7421/mcp --token-file "$AGENTFS_DIRECTORY/token" tools
```

| Option / environment | Behavior |
| --- | --- |
| `--endpoint` / `AGENTFS_ENDPOINT` | Full MCP URL, including `/mcp`; default `http://127.0.0.1:7421/mcp`. The option overrides the environment. |
| `--token-file` / `AGENTFS_TOKEN_FILE` | Reads a token file and trims surrounding whitespace. The option overrides the environment. |
| `AGENTFS_TOKEN` | Token fallback when no token file is selected. A selected but unreadable token file causes an error. |
| `RUST_LOG` | Log filter; the client defaults to `warn`. Logs use stderr. |

The daemon must already be running. Host paths in tool arguments, such as import sources and mount paths, refer to the daemon machine. `--token-file` and standard input refer to the client machine.

`tools` prints the MCP catalog object with a `tools` array. `call` prints the tool's `structuredContent` as JSON. Errors have `error.code`, `error.message`, and `error.retryable`; an operation receipt can contain both a saved result and an error. For operations, inspect `phase`, `local_saved`, `remote_confirmed`, and `mount_ready` independently.

The client exits with 0 when the tool response has no error, 2 for a tool error, and 1 for transport, authentication, token-reading, or JSON input errors. Command-line parsing errors also use exit code 2. Exit code 0 can accompany `waiting_for_quiesce`; inspect the receipt before continuing dependent work.

`--json -` reads standard input, with an 8 MiB input limit. The HTTP request body also has an 8 MiB limit, including its MCP envelope. `--request-id` wraps the supplied JSON as `{ "request_id": "...", "request": { ... } }`. Use it for the command tools listed in the [MCP reference](mcp.md). Queries and `operation_cancel` receive direct arguments. Keep the same request ID and arguments for retries; a new intended operation needs a new request ID.

## Create, import, read, and export

In another terminal, set the same `PATH` and `AGENTFS_DIRECTORY` as above. Use a fresh `RUN_ID` and source/export paths for a separate demonstration. This example works with the default macOS build and does not require a native mount.

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

Creation returns `result.workspace`. Import creates a new branch and returns `result.branch`; its `formal_head` is the imported revision. The protected `main` branch accepts validated merges. `revision_read` returns `{ "bytes": [0, 1, ...], "length": ... }`, with integer bytes and a maximum requested size of 4 MiB. Export requires an existing parent directory and a destination that does not already exist.

## Sessions and turns

Continue in the same terminal using the workspace, revision, and status from the preceding example:

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

This creates and closes an unmounted session. To give an agent a native directory, supply an absolute `mount_path` within a configured `mount_roots` directory to `session_open`, with the platform driver enabled. Wait for `mount_ready: true` before starting the agent. Use a new session ID for another session. Paused or interrupted sessions use `session_resume`; mount release and recovery are described in [operations](operations.md).

## Read a saved request from a file

JSON files contain the tool request fields. For example, `create-request.json` can contain `{ "name": "another-workspace" }`:

```sh
agentfs call workspace_create --request-id create-from-file --json - < create-request.json
```

In PowerShell:

```powershell
$env:AGENTFS_TOKEN_FILE = 'C:\Users\alice\agentfs\token'
Get-Content -Raw .\create-request.json | .\target\debug\agentfs.exe call workspace_create --request-id create-from-file --json -
```

Use [S3 configuration](configuration.md) for push/pull examples and [MCP parameters](mcp.md) for guards, cache ranges, merges, and receipt recovery.
