# Architecture

`agentfsd` is the composition root. It creates concrete adapters, injects them into the engine, and serves the resulting `Services` facade. The engine imports model and port crates; persistence, native drivers, S3, and protocol dependencies stay in adapters.

| Crate | Responsibility |
| --- | --- |
| `agentfs-model` | IDs, paths, objects, revisions, branches, sessions, operations, merge records, errors |
| `agentfs-ports` | Storage, filesystem, mount, clock, validation, directory, and application interfaces |
| `agentfs-engine` | File semantics and workspace, revision, session, replication, cache, ownership, merge workflows |
| `agentfs-local` | A process lock, SQLite metadata, immutable objects, working files, leases, verified reads |
| `agentfs-s3` | Streaming immutable object transport and conditional reference updates |
| `agentfs-platform` | FUSE/WinFsp callbacks, contained host-directory access, container validation, clock |
| `agentfs-mcp` | Typed tools, authenticated HTTP routing, client, stdio forwarding |
| `agentfs-test-support` | Deterministic clocks, mount and storage doubles, injected outages and lost acknowledgements |
| `agentfsd` / `agentfs` | Daemon lifecycle and command-line client |

## Local persistence

Each daemon state directory contains `engine.lock`, `meta.db`, SQLite WAL files, `objects/`, `staging/`, and `working/`. A process lock permits one writer. SQLite uses WAL and `synchronous=FULL`. Persisted format versions are checked before schema changes. Storage errors propagate as typed errors.

Object IDs hash a domain separator, format version, object kind, and bytes with SHA-256. Publication verifies the digest and size, flushes the temporary file, publishes its immutable name, and registers it. Unix directories are flushed after publication; Windows uses a write-through file move. A metadata transaction references installed immutable objects. Reader leases protect open content from cache eviction.

A `TreeRoot` refers to bounded directory and inode indexes. Index pages contain at most 128 entries and metadata objects are bounded by 2 MiB. File content is streamed with bounded buffers. Directory entry names are NFC normalized and compared using Unicode case folding. Portable-name checks reject traversal, reserved Windows names, trailing dots/spaces, and nonportable characters.

Each branch has a serialization gate covering file mutation and snapshot capture. A capture seals changed files, builds immutable indexes, then commits the root, revision, turn/session result, operation receipt, and synchronization job in one local transaction. A filesystem `fsync` confirms a local snapshot. Autosave uses a five-second quiet period and a thirty-second continuous-write deadline; the daemon checks once per second.

A lifecycle gate serializes session and mount changes, restore, merge apply, ownership transfer, and cancellation. File handles carry mount and branch generations. Generation changes reject stale handles. A mount path has one active binding, and a branch has at most one writable mount.

## Replication and ownership

A workspace has immutable objects plus a conditional workspace control record and conditional branch references. A branch reference contains its owner, epoch, generation, source generation, heads, and immutable history index. The history index records revisions, turn results, and operation receipts.

Publication uploads the referenced closure before compare-and-swap of the branch reference. Opaque provider versions are used for conditional writes. When an acknowledgement is lost, the publisher inspects the original operation's history record. Durable local jobs survive retries and process restarts. Workspace control records protect in-progress publication/read roots and coordinate the GC phase protocol. Physical remote deletion is disabled.

Pull imports remote history and references without changing an active local branch. Explicit adoption follows a confirmed ownership transfer. Transfer stops the old writer, drains its publication queue, then conditionally advances owner and epoch. The new location adopts the confirmed state. An unreachable owner cannot be forcibly replaced by this API.

## Merge

Merge preparation selects a common ancestor or validates an explicit one, compares path changes, and produces an immutable candidate. Changes to the same path require explicit resolution. Resolution creates a new candidate. Validation records belong to a specific candidate and target state; stale records cannot authorize apply. Apply requires clean target state, current ownership/generation/head, completed validation, and mount quiescence. The resulting revision retains source and target parents.

Administrator-configured container validations export candidate content into a private directory and run a preinstalled image with no network, a read-only filesystem and input mount, an unprivileged user, dropped capabilities, and resource/time/output limits. API callers select a configuration name. They cannot supply arbitrary host commands.

## Authentication boundary

Bearer credentials select a principal before creating the MCP service. Each principal has a separate HTTP session manager. Tool arguments cannot override the principal. Host and Origin allowlists and an 8 MiB request limit apply at the HTTP boundary. The daemon binds to loopback. Remote access requires an administrator-managed authenticated TLS proxy preserving the accepted Host value.

Workspace grants authorize application actions. Native filesystem access is tied to the daemon's OS identity. Keep configuration, credentials, state, mount roots, and exchange roots restricted to that identity; on Windows configure their NTFS permissions accordingly. S3 credentials and administrators are trusted storage writers.
