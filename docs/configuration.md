# Configuration and S3

[English](configuration.md) | [简体中文](configuration.zh-CN.md)

## Daemon configuration

Run `agentfsd init --directory /absolute/path/to/agentfs` to create `agentfs.json`, a private token file, and the mount/exchange directories. Edit the generated configuration, then start `agentfsd serve --config /absolute/path/to/agentfs/agentfs.json`. Configuration is read at startup; changes require a daemon restart.

| Field | Meaning and default |
| --- | --- |
| `data_dir` | Required absolute path for local state; initialization selects `<directory>/state`. One daemon per state directory. |
| `listen` | Required loopback socket address; initialization selects `127.0.0.1:7421`. |
| `mount_roots` | Required array of allowed absolute mount roots; initialization creates `<directory>/mounts`. |
| `directory_roots` | Allowed absolute import/export roots; omitted means `[]`. Initialization creates and permits `<directory>/exchange`. |
| `credentials` | Required array of `{ "principal": "owner", "token_file": "/absolute/path/token" }`. At least one credential is required. |
| `allowed_origins` | HTTP Origin allowlist, default `[]`. CLI and stdio connections do not send an Origin. |
| `cache` | Optional object; defaults to `max_bytes: 2147483648`, `target_bytes: 1610612736`, `min_free_bytes: 67108864`. If supplied, include all three fields; `target_bytes` must be below `max_bytes`. |
| `validation` | Map of named container checks, default `{}`; see [operations](operations.md). |
| `docker` | Docker executable name or path, default `docker`. |
| `remote` | S3 configuration below. Omitted or `null` means local storage only. |

All state, root, and token paths must be absolute. Create allowed roots before starting the daemon. JSON strings do not expand `~`, `$HOME`, or environment variables. Windows paths use escaped backslashes, such as `"C:\\Users\\alice\\agentfs\\state"`, or forward slashes. Unknown top-level configuration fields and unknown `remote` fields are rejected. The configuration file is limited to 1 MiB.

Token files must be regular files, at most 4096 bytes, containing at least 32 non-whitespace bytes after trimming. On Unix, group and other permissions must be absent; initialization creates mode `0600`. Tokens must be unique. A token selects the configured `principal`; workspace ownership and grants determine that principal's access. Protect configuration and state using the daemon user's filesystem permissions.

## S3 fields

The following object belongs under the **top-level `remote` key** in `agentfs.json`:

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

This is a configuration fragment for a local RustFS S3 API endpoint. Keep the other generated fields. `endpoint` is the S3 API base URL, without the bucket name, credentials, query parameters, or a fragment. The adapter uses path-style requests and appends the bucket name. Use the S3 API port supplied by your deployment.

| `remote` field | Required / default | Behavior |
| --- | --- | --- |
| `bucket` | Required, nonempty string | Existing bucket name. The daemon does not create buckets. |
| `prefix` | `agentfs/v1` | Shared namespace within the bucket; leading/trailing `/` are removed and the result must be a valid, nonempty object path. |
| `region` | `us-east-1` | Signing region; use the bucket's region for AWS S3. |
| `endpoint` | Omitted or `null` | Optional custom S3 base URL. With no endpoint override, the dependency derives the AWS regional endpoint. |
| `allow_http` | `false` | Set `true` for an explicitly selected HTTP development endpoint. HTTPS endpoints keep `false`. |

For AWS S3, replace the bucket and region, and omit a custom endpoint:

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

## Credentials and environment precedence

The **daemon process** reads S3 credentials. Set these variables in the shell or service environment that starts `agentfsd serve`. `AGENTFS_TOKEN_FILE` authenticates the CLI to AgentFS separately.

```sh
export AWS_ACCESS_KEY_ID='your-access-key-id'
export AWS_SECRET_ACCESS_KEY='your-secret-access-key'
# Temporary credentials also require AWS_SESSION_TOKEN.
./target/debug/agentfsd serve --config "$AGENTFS_DIRECTORY/agentfs.json"
```

PowerShell uses `$env:AWS_ACCESS_KEY_ID = 'your-access-key-id'` and `$env:AWS_SECRET_ACCESS_KEY = 'your-secret-access-key'` before starting `agentfsd.exe`.

The locked `object_store` dependency supports the following credential sources, in this order:

1. `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`, with optional `AWS_SESSION_TOKEN`. Supply both key fields together.
2. `AWS_WEB_IDENTITY_TOKEN_FILE` and `AWS_ROLE_ARN`, with optional `AWS_ROLE_SESSION_NAME`.
3. `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` for a container task role.
4. `AWS_CONTAINER_CREDENTIALS_FULL_URI` together with `AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE` for EKS Pod Identity.
5. Instance metadata credentials when the preceding sources are absent.

The current adapter does not load `~/.aws/credentials`, `AWS_PROFILE`, or AWS CLI SSO sessions. Export resolved credentials or use one of the supported role providers. Static environment credentials are loaded at startup; restart the daemon after replacing them.

AgentFS calls `AmazonS3Builder::from_env()` and then applies its configuration. The configured `bucket`, `region`, and `allow_http` values, including their defaults, override the corresponding AWS environment settings. A configured `endpoint` overrides `AWS_ENDPOINT`. The dependency's `AWS_ENDPOINT_URL_S3` takes precedence over both. When selecting the endpoint through `agentfs.json`, remove any unintended `AWS_ENDPOINT_URL_S3`; also remove `AWS_ENDPOINT` when using the default AWS regional endpoint. Path-style requests and ETag conditional writes are selected by the adapter.

## Storage requirements and verification

The storage service must support GET/HEAD, listing by prefix, object writes, multipart upload and abort, and strongly consistent reads. AgentFS creates immutable objects with `If-None-Match: *`, completes multipart uploads with the same condition, and updates references using `If-Match` with the previous ETag. Conditional write behavior is described in the [AWS S3 documentation](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html). Files larger than 8 MiB use multipart upload; part sizes range from 8 to 64 MiB according to file size. Restrict storage credentials to the selected bucket and namespace.

Daemon startup constructs the S3 client; it does not prove that the bucket or credentials work. After importing content using the [CLI example](cli.md), explicitly push and inspect the original import receipt:

```sh
jq -n --arg workspace "$WORKSPACE" '{workspace:$workspace,direction:"push"}' |
  agentfs call sync --request-id "$RUN_ID-push" --json -
jq -n --arg operation "$(jq -r '.id' "$AGENTFS_DIRECTORY/demo-import.json")" '{operation:$operation}' |
  agentfs call operation_get --json -
```

Check `remote_confirmed` on the original operation. `workspace_status.capabilities.remote_configured` reports configuration presence, and `pending_jobs` reports the queue length. The daemon retries pending synchronization every three seconds. A `local_saved: true` receipt can still contain a remote error; retain its operation ID and follow [recovery guidance](operations.md).

For a compatibility check covering conditional writes, competing updates, 20 MiB multipart content, and two-daemon replication, use the isolated [S3/RustFS tests](validation.md). Those tests use a separate test bucket and are not a production health check.

## Multiple machines

Configure each daemon with the same S3 endpoint, bucket, prefix, and signing region. Each machine needs its own initialized state directory and `LocationId`; share the workspace ID returned by the source daemon. The destination's authenticated principal must be the workspace owner or have suitable grants. Tokens can differ between machines.

Push on the source, then call `sync` with `direction: "pull"` and the same workspace ID on the destination. Pull imports history and remote branch references. Use a retained revision to create a local branch or session, or complete an online `ownership_transfer` before adopting an existing writable branch. See [operations](operations.md) and the [MCP reference](mcp.md).

Configuration definitions: [daemon](../apps/agentfsd/src/config.rs), [S3 adapter](../crates/agentfs-s3/src/lib.rs), and dependency version in [Cargo.lock](../Cargo.lock).
