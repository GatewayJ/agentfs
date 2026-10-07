# Validation

Run all standard checks through multicore Make:

```sh
make -j "$(getconf _NPROCESSORS_ONLN)" verify
make -j "$(getconf _NPROCESSORS_ONLN)" build
python3 tests/cli_smoke.py
```

The workflow tests exercise concurrent request retries, atomic append, rename with open handles, restart recovery, immutable turn outcomes, autosave timing, remote failure and lost acknowledgements, ownership transfer, merge validation, stale mount changes, cache pinning/eviction, and offline prefetch. Persistence tests cover digest failures, reader leases, transaction rollback, request recovery, and unknown format rejection. MCP tests verify concurrent clients, principal isolation, authentication, and Host/Origin policy. The CLI test starts real processes and verifies stdio MCP, import/export content and metadata, allowed paths, and process restart.

## Native filesystems

Install and enable the platform driver, then run:

```sh
make -j "$(getconf _NPROCESSORS_ONLN)" test CARGO_FLAGS='-p agentfs-platform --test native_mount -- --ignored --nocapture'
```

On macOS add `--features macos-mount` inside `CARGO_FLAGS`. On Windows use `make -j $env:NUMBER_OF_PROCESSORS` in PowerShell. The integration test owns its temporary state and mount directories. It tests a file over 3 MiB, truncation, random write, atomic append, rename, fsync, over 128 directory entries, historical read-only access, and unmount.

The WinFsp wrapper's null volume-flush regression is tested separately on Windows:

```powershell
make -j $env:NUMBER_OF_PROCESSORS test CARGO_FLAGS='--manifest-path vendor/winfsp_wrs/Cargo.toml --lib'
```

## Container validation

With Docker and `debian:bookworm-slim` available:

```sh
make -j "$(getconf _NPROCESSORS_ONLN)" test CARGO_FLAGS='-p agentfs-platform --test validation -- --ignored --nocapture'
```

The test runs as an unprivileged container user, reads privately permissioned candidate content, verifies the input mount is read-only, and checks output and time limits. Candidate materialization grants read access to the validation user while retaining executable-file behavior; container mount permissions prevent modification.

## S3 / RustFS

Use a disposable endpoint and credentials permitted to create the `agentfs-integration` bucket. Test objects use unique prefixes. Do not point these tests at production storage.

```sh
export AGENTFS_TEST_S3_ENDPOINT=http://127.0.0.1:9000
export AWS_ACCESS_KEY_ID=AGENTFSTESTLOCAL
export AWS_SECRET_ACCESS_KEY=agentfs-local-integration-secret-2026
make -j "$(getconf _NPROCESSORS_ONLN)" test CARGO_FLAGS='-p agentfs-s3 -- --ignored --nocapture'
python3 tests/replica_smoke.py
```

Backend tests exercise competing conditional reference creation, stale conditional updates, empty files, digest rejection, and 20 MiB multipart object publication. The two-daemon test pushes an imported revision, pulls and exports it at another location, and transfers branch ownership through the real S3 backend.

## Execution evidence

| Check | Verified environment |
| --- | --- |
| Workspace tests and Clippy | macOS, Linux, Windows |
| Native mounted file operations and revision access | Linux as root and as an ordinary user; Windows with WinFsp 2.1 |
| CLI, stdio MCP, directory metadata, restart recovery | macOS, Linux, Windows |
| Conditional S3 writes and multipart content | Isolated RustFS instance |
| Two-daemon S3 replication and ownership transfer | macOS and Linux |
| Container validation permissions, output limits, timeout | Docker on macOS |
| macFUSE callback compilation | macOS |

[GitHub Actions](https://github.com/GatewayJ/agentfs/actions) runs the checked-in test matrix for each commit, including container validation on Linux. Native macOS mounting requires an installed and enabled macFUSE driver; that kernel mount test has not been executed in the development environment.
