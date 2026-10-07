CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value NOT NULL);
CREATE TABLE IF NOT EXISTS workspaces (id TEXT PRIMARY KEY, data BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS branches (id TEXT PRIMARY KEY, workspace TEXT NOT NULL, data BLOB NOT NULL);
CREATE INDEX IF NOT EXISTS branches_workspace ON branches(workspace);
CREATE TABLE IF NOT EXISTS nodes (
    branch TEXT NOT NULL,
    path TEXT NOT NULL,
    inode INTEGER NOT NULL,
    data BLOB NOT NULL,
    PRIMARY KEY(branch, path),
    UNIQUE(branch, inode),
    FOREIGN KEY(branch) REFERENCES branches(id)
);
CREATE TABLE IF NOT EXISTS revisions (id TEXT PRIMARY KEY, workspace TEXT NOT NULL, data BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, workspace TEXT NOT NULL, data BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS turns (id TEXT PRIMARY KEY, workspace TEXT NOT NULL, data BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS operations (
    id TEXT PRIMARY KEY,
    principal TEXT NOT NULL,
    request_id TEXT NOT NULL,
    workspace TEXT,
    data BLOB NOT NULL,
    UNIQUE(principal, request_id)
);
CREATE TABLE IF NOT EXISTS mounts (
    id TEXT PRIMARY KEY,
    path TEXT NOT NULL,
    branch TEXT NOT NULL,
    active INTEGER NOT NULL,
    writable INTEGER NOT NULL,
    data BLOB NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS mounts_path ON mounts(path) WHERE active=1;
CREATE UNIQUE INDEX IF NOT EXISTS mounts_writer ON mounts(branch) WHERE writable=1;
CREATE TABLE IF NOT EXISTS merges (id TEXT PRIMARY KEY, workspace TEXT NOT NULL, data BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS sync_jobs (
    id TEXT PRIMARY KEY,
    workspace TEXT NOT NULL,
    branch TEXT NOT NULL,
    source_generation INTEGER NOT NULL,
    data BLOB NOT NULL,
    UNIQUE(branch, source_generation)
);
CREATE TABLE IF NOT EXISTS remote_refs (id TEXT PRIMARY KEY, workspace TEXT NOT NULL, data BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS cache_pins (id TEXT PRIMARY KEY, workspace TEXT NOT NULL, data BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS objects (
    workspace TEXT NOT NULL,
    hash TEXT NOT NULL,
    kind TEXT NOT NULL,
    size INTEGER NOT NULL,
    remote_confirmed INTEGER NOT NULL DEFAULT 0,
    last_access_ns INTEGER NOT NULL,
    PRIMARY KEY(workspace, hash)
);
