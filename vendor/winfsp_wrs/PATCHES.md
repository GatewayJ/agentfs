# Local changes to winfsp_wrs 0.4.1

Upstream: https://github.com/Scille/winfsp_wrs (MIT). Source comes from the crates.io 0.4.1 release.

The Flush callback receives a null file context for a whole-volume flush. The wrapper represents this as `Option<FileContext>` and writes returned file information only when its output pointer is non-null. This preserves the WinFsp volume-flush API and prevents passing a null pointer to `Arc::increment_strong_count`. AgentFS flushes the branch for both file and volume requests.
