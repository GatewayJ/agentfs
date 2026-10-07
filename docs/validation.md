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

During repository creation, macOS workspace tests and Clippy, Windows cross-compilation of the native adapter, macFUSE callback compilation, real Linux FUSE mounting in an isolated container, and real RustFS backend tests passed. CLI/stdio tests passed on macOS. GitHub Actions defines native Linux/Windows runs and macOS checks; its results are the evidence for those hosted environments. A macOS native mount run requires an enabled local macFUSE installation.
