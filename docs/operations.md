# Operations and recovery

## Request identity and receipts

Reuse a `request_id` for the same tool and identical arguments. Identity is scoped to the authenticated principal and persists across restarts. A conflicting reuse returns `REQUEST_ID_CONFLICT`. Requests reserve their operation before effects. `operation_by_request` and `operation_get` recover the original receipt.

Read these receipt fields independently:

| Field | Meaning |
| --- | --- |
| `local_saved` | The operation's result and referenced local state have been committed |
| `remote_confirmed` | Remote publication of the operation has been confirmed |
| `mount_ready` | The requested native attachment is ready, when relevant |
| `phase` | Current workflow state |
| `error` | A typed failure, possibly alongside a completed local save |

A remote error can accompany `local_saved: true`. Keep the existing operation ID, restore remote access, and run `sync` or allow the daemon to retry its persistent publication queue. A remote timeout does not establish whether a conditional write committed; the publisher checks the recorded operation identity.

## Mount changes

Restore, merge apply, and session changes may require an attached filesystem to become quiet. An operation in `waiting_for_quiesce` carries its original mount generation. Stop the agent and other processes using that mount, close open files, and leave the directory in any shell. Call `mount_release` with that operation ID and `expected_binding_generation`. Repeating release uses its own stable request ID.

The operation verifies the original branch guard again before changing the attachment. A stale target fails without applying the candidate. Generation checks prevent a handle from continuing against a replacement branch. A successful local commit followed by mount failure returns `committed_mount_pending`; the durable result is retained for recovery. Cancellation is accepted only while the original attachment remains unchanged and no local commit has completed.

A normal daemon shutdown stops HTTP traffic and background tasks, saves modified local branches, and requests ordinary unmounts. Busy mounts are reported for recovery. Do not remove the state directory while a daemon or filesystem is active.

## Process restart

Startup restores working metadata from the last acknowledged immutable branch root and removes disposable working copies. Writes that had not reached fsync, autosave, a revision operation, or a saved turn can be lost after abrupt process termination. A failed write followed by failed metadata persistence stops the affected branch for recovery.

An interrupted session requires `session_resume` with an explicit `recovery_action` and the affected `turn_id` when an active turn exists. `continue` resumes the recorded turn; `interrupt` records interruption before resuming. Native mounts are inspected before reuse. An unknown existing filesystem at a path requires operator cleanup; the service does not force-unmount an unrelated filesystem.

Keep the state directory on a local filesystem with correct file flush and atomic rename behavior. Filesystem and storage hardware must honor flush requests. Unix object publication flushes file data and directories; Windows flushes file data and uses write-through namespace publication. Remote confirmation is the additional durability boundary when remote storage is configured.

## Replication, cache, and ownership

`sync` supports `push`, `pull`, and `both`. Pulling a workspace onto a fresh location imports its history. Branch creation and session opening can then use the imported revisions. A remote history reference does not grant local write ownership.

Use `cache_prefetch` to download a revision or selected paths. Use `cache_pin` with `enabled: true` to retain a complete selection for offline use; `cache_status` reports both completeness and pin protection. Unpinning permits later collection after other protection is absent. Cache collection only removes remotely confirmed immutable objects without active readers or protected references.

Ownership transfer is online. Finish active turns and release writable mounts on the old location. Submit `ownership_transfer` with the current `BranchGuard` and the destination `LocationId`. The old location stops branch writes before draining publication and updating the conditional reference. Pull at the new location to adopt confirmed ownership. Retain the original transfer operation during an uncertain remote response.

## Merge validation

`merge_prepare` produces a candidate and path conflicts. Submit `merge_resolve` against the current candidate using source, target, delete, or replacement content for each conflict. `merge_validate` performs structural checks and can run an administrator-named container configuration. Any candidate change invalidates earlier validation. `merge_apply` includes the validated candidate and current target guard.

Example configuration entry under `validation`:

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

Preload the selected image; validation uses `--pull=never`. Use an immutable image digest for controlled deployments. The candidate is mounted at `/workspace` read-only; scratch space is `/tmp`.
