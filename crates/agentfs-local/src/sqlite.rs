use crate::LocalBackend;
use agentfs_model::*;
use agentfs_ports::*;
use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, Transaction, params, params_from_iter};
use serde::{Serialize, de::DeserializeOwned};

pub(crate) fn sql_error(error: rusqlite::Error) -> Error {
    let code = match error.sqlite_error_code() {
        Some(rusqlite::ErrorCode::ConstraintViolation) => ErrorCode::Conflict,
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
            ErrorCode::Busy
        }
        Some(rusqlite::ErrorCode::DiskFull) => ErrorCode::CapacityExceeded,
        Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase) => {
            ErrorCode::Integrity
        }
        _ => ErrorCode::Io,
    };
    Error::new(code, error.to_string())
}

pub(crate) fn initialize(connection: &mut Connection, version: u32) -> Result<LocationId> {
    validate_format(connection, version)?;
    let transaction = connection.transaction().map_err(sql_error)?;
    transaction
        .execute_batch(include_str!("schema.sql"))
        .map_err(sql_error)?;
    transaction
        .execute(
            "INSERT OR IGNORE INTO metadata(key,value) VALUES('format_version',?1)",
            [version],
        )
        .map_err(sql_error)?;
    let location: Option<String> = transaction
        .query_row(
            "SELECT value FROM metadata WHERE key='location_id'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    let location = match location {
        Some(value) => value.parse()?,
        None => {
            let id = LocationId::new();
            transaction
                .execute(
                    "INSERT INTO metadata(key,value) VALUES('location_id',?1)",
                    [id.to_string()],
                )
                .map_err(sql_error)?;
            id
        }
    };
    transaction.commit().map_err(sql_error)?;
    Ok(location)
}

pub(crate) fn validate_format(connection: &Connection, version: u32) -> Result<()> {
    let exists: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='metadata')",
            [],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    if exists {
        let stored: Option<u32> = connection
            .query_row(
                "SELECT value FROM metadata WHERE key='format_version'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_error)?;
        if stored != Some(version) {
            return Err(Error::new(
                ErrorCode::Unsupported,
                "unsupported local database format",
            ));
        }
    }
    Ok(())
}

pub(crate) fn db_integer(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::invalid("value exceeds the persistent integer range"))
}

pub(crate) fn unsigned_integer(value: i64) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| Error::integrity("database contains a negative unsigned integer"))
}

fn read_one<T: DeserializeOwned>(
    connection: &Connection,
    sql: &str,
    args: &[String],
) -> Result<Option<T>> {
    let bytes: Option<Vec<u8>> = connection
        .query_row(sql, params_from_iter(args), |row| row.get(0))
        .optional()
        .map_err(sql_error)?;
    bytes.map(|value| decode(&value)).transpose()
}

fn read_many<T: DeserializeOwned>(
    connection: &Connection,
    sql: &str,
    args: &[String],
) -> Result<Vec<T>> {
    let mut statement = connection.prepare(sql).map_err(sql_error)?;
    let rows = statement
        .query_map(params_from_iter(args), |row| row.get::<_, Vec<u8>>(0))
        .map_err(sql_error)?;
    rows.map(|row| decode(&row.map_err(sql_error)?)).collect()
}

impl LocalBackend {
    async fn one<T: DeserializeOwned + Send + 'static>(
        &self,
        sql: &'static str,
        args: Vec<String>,
    ) -> Result<Option<T>> {
        self.db(move |connection| read_one(connection, sql, &args))
            .await
    }
    async fn many<T: DeserializeOwned + Send + 'static>(
        &self,
        sql: &'static str,
        args: Vec<String>,
    ) -> Result<Vec<T>> {
        self.db(move |connection| read_many(connection, sql, &args))
            .await
    }
}

fn session_key(key: &SessionKey) -> Result<String> {
    Ok(object_ref(ObjectKind::RecordIndex, &encode(key)?)
        .id
        .to_string())
}

fn pin_key(pin: &CachePin) -> Result<String> {
    Ok(object_ref(
        ObjectKind::RecordIndex,
        &encode(&(pin.workspace, pin.revision, &pin.paths))?,
    )
    .id
    .to_string())
}

fn ensure_root(
    transaction: &Transaction<'_>,
    workspace: WorkspaceId,
    root: &ObjectRef,
) -> Result<()> {
    let found: bool = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM objects WHERE workspace=?1 AND hash=?2 AND size=?3)",
            params![
                workspace.to_string(),
                root.id.as_str(),
                db_integer(root.size)?
            ],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    if !found {
        return Err(Error::integrity(
            "a local commit refers to an uninstalled root object",
        ));
    }
    Ok(())
}

fn check_guard(transaction: &Transaction<'_>, guard: &BranchGuard) -> Result<()> {
    let branch: Branch = read_one(
        transaction,
        "SELECT data FROM branches WHERE id=?1",
        &[guard.branch.to_string()],
    )?
    .ok_or_else(|| Error::new(ErrorCode::NotFound, "branch does not exist"))?;
    if branch.authority_epoch != guard.authority_epoch {
        return Err(Error::new(
            ErrorCode::StaleAuthority,
            "branch authority changed before commit",
        ));
    }
    if branch.generation != guard.generation
        || branch.formal_head != guard.expected_head
        || guard
            .mutation_seq
            .is_some_and(|expected| expected != branch.mutation_seq)
    {
        return Err(Error::new(
            ErrorCode::TargetMoved,
            "branch changed before commit",
        ));
    }
    Ok(())
}

fn upsert<T: Serialize>(
    transaction: &Transaction<'_>,
    table: &str,
    id: String,
    workspace: WorkspaceId,
    value: &T,
) -> Result<()> {
    let sql = format!(
        "INSERT INTO {table}(id,workspace,data) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET data=excluded.data"
    );
    transaction
        .execute(&sql, params![id, workspace.to_string(), encode(value)?])
        .map_err(sql_error)?;
    Ok(())
}

fn put_node(transaction: &Transaction<'_>, branch: BranchId, node: &Node) -> Result<()> {
    transaction.execute("INSERT INTO nodes(branch,path,inode,data) VALUES(?1,?2,?3,?4) ON CONFLICT(branch,path) DO UPDATE SET inode=excluded.inode,data=excluded.data",
        params![branch.to_string(), node.path.lookup_key(), db_integer(node.inode.id.0)?, encode(node)?]).map_err(sql_error)?;
    Ok(())
}

#[async_trait]
impl LocalStateStore for LocalBackend {
    async fn location(&self) -> Result<LocationId> {
        Ok(self.inner.location)
    }
    async fn workspace(&self, id: WorkspaceId) -> Result<Option<Workspace>> {
        self.one(
            "SELECT data FROM workspaces WHERE id=?1",
            vec![id.to_string()],
        )
        .await
    }
    async fn workspaces(&self) -> Result<Vec<Workspace>> {
        self.many("SELECT data FROM workspaces ORDER BY id", vec![])
            .await
    }
    async fn branch(&self, id: BranchId) -> Result<Option<Branch>> {
        self.one(
            "SELECT data FROM branches WHERE id=?1",
            vec![id.to_string()],
        )
        .await
    }
    async fn branches(&self, workspace: WorkspaceId) -> Result<Vec<Branch>> {
        self.many(
            "SELECT data FROM branches WHERE workspace=?1 ORDER BY id",
            vec![workspace.to_string()],
        )
        .await
    }
    async fn nodes(&self, branch: BranchId) -> Result<Vec<Node>> {
        self.many(
            "SELECT data FROM nodes WHERE branch=?1 ORDER BY path",
            vec![branch.to_string()],
        )
        .await
    }
    async fn node(&self, branch: BranchId, inode: InodeId) -> Result<Option<Node>> {
        self.one(
            "SELECT data FROM nodes WHERE branch=?1 AND inode=?2",
            vec![branch.to_string(), inode.0.to_string()],
        )
        .await
    }
    async fn node_at(&self, branch: BranchId, path: &WorkspacePath) -> Result<Option<Node>> {
        self.one(
            "SELECT data FROM nodes WHERE branch=?1 AND path=?2",
            vec![branch.to_string(), path.lookup_key()],
        )
        .await
    }
    async fn revision(&self, id: RevisionId) -> Result<Option<Revision>> {
        self.one(
            "SELECT data FROM revisions WHERE id=?1",
            vec![id.to_string()],
        )
        .await
    }
    async fn revisions(&self, workspace: WorkspaceId) -> Result<Vec<Revision>> {
        self.many(
            "SELECT data FROM revisions WHERE workspace=?1 ORDER BY id",
            vec![workspace.to_string()],
        )
        .await
    }
    async fn session(&self, key: &SessionKey) -> Result<Option<SessionBinding>> {
        self.one(
            "SELECT data FROM sessions WHERE id=?1",
            vec![session_key(key)?],
        )
        .await
    }
    async fn sessions(&self, workspace: WorkspaceId) -> Result<Vec<SessionBinding>> {
        self.many(
            "SELECT data FROM sessions WHERE workspace=?1 ORDER BY id",
            vec![workspace.to_string()],
        )
        .await
    }
    async fn turns(&self, workspace: WorkspaceId) -> Result<Vec<TurnRecord>> {
        self.many(
            "SELECT data FROM turns WHERE workspace=?1 ORDER BY id",
            vec![workspace.to_string()],
        )
        .await
    }
    async fn operation(&self, id: OperationId) -> Result<Option<OperationRecord>> {
        self.one(
            "SELECT data FROM operations WHERE id=?1",
            vec![id.to_string()],
        )
        .await
    }
    async fn operation_by_request(
        &self,
        actor: &Principal,
        request_id: &str,
    ) -> Result<Option<OperationRecord>> {
        self.one(
            "SELECT data FROM operations WHERE principal=?1 AND request_id=?2",
            vec![actor.as_str().to_owned(), request_id.to_owned()],
        )
        .await
    }
    async fn operations(&self, workspace: WorkspaceId) -> Result<Vec<OperationRecord>> {
        self.many(
            "SELECT data FROM operations WHERE workspace=?1 ORDER BY id",
            vec![workspace.to_string()],
        )
        .await
    }
    async fn mount(&self, id: MountId) -> Result<Option<MountBinding>> {
        self.one("SELECT data FROM mounts WHERE id=?1", vec![id.to_string()])
            .await
    }
    async fn mounts(&self) -> Result<Vec<MountBinding>> {
        self.many("SELECT data FROM mounts ORDER BY id", vec![])
            .await
    }
    async fn merge(&self, id: MergeId) -> Result<Option<MergeCandidate>> {
        self.one("SELECT data FROM merges WHERE id=?1", vec![id.to_string()])
            .await
    }
    async fn merges(&self, workspace: WorkspaceId) -> Result<Vec<MergeCandidate>> {
        self.many(
            "SELECT data FROM merges WHERE workspace=?1 ORDER BY id",
            vec![workspace.to_string()],
        )
        .await
    }
    async fn sync_jobs(&self, workspace: WorkspaceId) -> Result<Vec<SyncJob>> {
        self.many(
            "SELECT data FROM sync_jobs WHERE workspace=?1 ORDER BY branch,source_generation",
            vec![workspace.to_string()],
        )
        .await
    }
    async fn remote_refs(&self, workspace: WorkspaceId) -> Result<Vec<BranchRef>> {
        self.many(
            "SELECT data FROM remote_refs WHERE workspace=?1 ORDER BY id",
            vec![workspace.to_string()],
        )
        .await
    }
    async fn cache_pins(&self, workspace: WorkspaceId) -> Result<Vec<CachePin>> {
        self.many(
            "SELECT data FROM cache_pins WHERE workspace=?1 ORDER BY id",
            vec![workspace.to_string()],
        )
        .await
    }

    async fn reserve_operation(&self, record: OperationRecord) -> Result<ReservedOperation> {
        self.db(move |connection| {
            let transaction = connection.transaction().map_err(sql_error)?;
            let existing: Option<OperationRecord> = read_one(
                &transaction,
                "SELECT data FROM operations WHERE principal=?1 AND request_id=?2",
                &[
                    record.principal.as_str().to_owned(),
                    record.request_id.clone(),
                ],
            )?;
            if let Some(existing) = existing {
                if existing.fingerprint != record.fingerprint || existing.name != record.name {
                    return Err(Error::new(
                        ErrorCode::RequestIdConflict,
                        "request_id was already used with different parameters",
                    ));
                }
                return Ok(ReservedOperation {
                    record: existing,
                    created: false,
                });
            }
            put_operation(&transaction, &record)?;
            transaction.commit().map_err(sql_error)?;
            Ok(ReservedOperation {
                record,
                created: true,
            })
        })
        .await
    }

    async fn commit(&self, commit: LocalCommit) -> Result<()> {
        self.db(move |connection| {
            let transaction = connection.transaction().map_err(sql_error)?;
            for guard in &commit.guards { check_guard(&transaction, guard)?; }
            for workspace in &commit.new_workspaces {
                transaction.execute("INSERT INTO workspaces(id,data) VALUES(?1,?2)", params![workspace.id.to_string(), encode(workspace)?]).map_err(sql_error)?;
            }
            for workspace in &commit.workspaces {
                transaction.execute("UPDATE workspaces SET data=?2 WHERE id=?1", params![workspace.id.to_string(), encode(workspace)?]).map_err(sql_error)?;
            }
            for branch in &commit.new_branches {
                ensure_root(&transaction, branch.workspace, &branch.working_root)?;
                transaction.execute("INSERT INTO branches(id,workspace,data) VALUES(?1,?2,?3)", params![branch.id.to_string(), branch.workspace.to_string(), encode(branch)?]).map_err(sql_error)?;
            }
            for branch in &commit.branches {
                ensure_root(&transaction, branch.workspace, &branch.working_root)?;
                upsert(&transaction, "branches", branch.id.to_string(), branch.workspace, branch)?;
            }
            for change in &commit.node_changes {
                match change {
                    NodeChange::Put { branch, node } => put_node(&transaction, *branch, node)?,
                    NodeChange::Remove { branch, path } => {
                        transaction.execute("DELETE FROM nodes WHERE branch=?1 AND path=?2", params![branch.to_string(), path.lookup_key()]).map_err(sql_error)?;
                    }
                    NodeChange::Replace { branch, nodes } => {
                        transaction.execute("DELETE FROM nodes WHERE branch=?1", [branch.to_string()]).map_err(sql_error)?;
                        for node in nodes { put_node(&transaction, *branch, node)?; }
                    }
                }
            }
            for revision in &commit.revisions {
                ensure_root(&transaction, revision.workspace, &revision.root)?;
                let existing: Option<Revision> = read_one(&transaction, "SELECT data FROM revisions WHERE id=?1", &[revision.id.to_string()])?;
                if existing.as_ref().is_some_and(|old| old != revision) { return Err(Error::integrity("immutable revision was changed")); }
                upsert(&transaction, "revisions", revision.id.to_string(), revision.workspace, revision)?;
            }
            for session in &commit.sessions { upsert(&transaction, "sessions", session_key(&session.key)?, session.key.workspace, session)?; }
            for turn in &commit.turns {
                let key = object_ref(ObjectKind::RecordIndex, &encode(&(&turn.session, &turn.turn_id))?).id.to_string();
                let existing: Option<TurnRecord> = read_one(&transaction, "SELECT data FROM turns WHERE id=?1", std::slice::from_ref(&key))?;
                if existing.as_ref().is_some_and(|old| old.status.is_end() && old != turn) {
                    return Err(Error::new(ErrorCode::TurnResultConflict, "turn already has a different final result"));
                }
                upsert(&transaction, "turns", key, turn.session.workspace, turn)?;
            }
            for operation in &commit.operations { put_operation(&transaction, operation)?; }
            for mount in &commit.mounts {
                let active = mount.state != MountState::Released;
                let writable = active && mount.access == AccessMode::ReadWrite;
                transaction.execute("INSERT INTO mounts(id,path,branch,active,writable,data) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO UPDATE SET path=excluded.path,branch=excluded.branch,active=excluded.active,writable=excluded.writable,data=excluded.data",
                    params![mount.id.to_string(), mount.path.to_string_lossy(), mount.branch.to_string(), active, writable, encode(mount)?]).map_err(sql_error)?;
            }
            for merge in &commit.merges { upsert(&transaction, "merges", merge.id.to_string(), merge.workspace, merge)?; }
            for job in &commit.sync_jobs {
                transaction.execute("INSERT INTO sync_jobs(id,workspace,branch,source_generation,data) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET data=excluded.data",
                    params![job.operation.to_string(), job.reference.workspace.to_string(), job.reference.branch.to_string(), db_integer(job.reference.source_generation)?, encode(job)?]).map_err(sql_error)?;
            }
            for id in &commit.completed_jobs { transaction.execute("DELETE FROM sync_jobs WHERE id=?1", [id.to_string()]).map_err(sql_error)?; }
            for reference in &commit.remote_refs {
                let existing: Option<BranchRef> = read_one(&transaction, "SELECT data FROM remote_refs WHERE id=?1", &[reference.branch.to_string()])?;
                if existing.as_ref().is_some_and(|old| old.source_generation > reference.source_generation) { continue; }
                if existing.as_ref().is_some_and(|old| old.source_generation == reference.source_generation && old != reference) {
                    return Err(Error::integrity("remote reference reused a publication sequence"));
                }
                upsert(&transaction, "remote_refs", reference.branch.to_string(), reference.workspace, reference)?;
            }
            for update in &commit.pin_updates {
                let key = pin_key(&update.pin)?;
                if update.enabled { upsert(&transaction, "cache_pins", key, update.pin.workspace, &update.pin)?; }
                else { transaction.execute("DELETE FROM cache_pins WHERE id=?1", [key]).map_err(sql_error)?; }
            }
            transaction.commit().map_err(sql_error)?;
            Ok(())
        }).await
    }
}

fn put_operation(transaction: &Transaction<'_>, record: &OperationRecord) -> Result<()> {
    let existing: Option<OperationRecord> = read_one(
        transaction,
        "SELECT data FROM operations WHERE id=?1",
        &[record.id.to_string()],
    )?;
    if existing.as_ref().is_some_and(|old| {
        old.principal != record.principal
            || old.request_id != record.request_id
            || old.fingerprint != record.fingerprint
            || old.name != record.name
    }) {
        return Err(Error::new(
            ErrorCode::RequestIdConflict,
            "operation identity is immutable",
        ));
    }
    transaction.execute("INSERT INTO operations(id,principal,request_id,workspace,data) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET workspace=excluded.workspace,data=excluded.data",
        params![record.id.to_string(), record.principal.as_str(), record.request_id, record.workspace.map(|id| id.to_string()), encode(record)?]).map_err(sql_error)?;
    Ok(())
}
