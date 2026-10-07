use agentfs_model::*;
use agentfs_ports::*;
use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Weak},
};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, RwLock};

/// Shared ordering for all services operating on one local database.
pub struct Runtime {
    pub state: Arc<dyn LocalStateStore>,
    pub clock: Arc<dyn Clock>,
    pub location: LocationId,
    pub(crate) objects: RwLock<()>,
    pub(crate) lifecycle: AsyncMutex<()>,
    pub(crate) stopped: Mutex<BTreeSet<BranchId>>,
    branches: Mutex<BTreeMap<BranchId, Weak<AsyncMutex<()>>>>,
    pub(crate) dirty: Mutex<BTreeMap<BranchId, DirtyTime>>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DirtyTime {
    pub first: u64,
    pub last: u64,
    pub sequence: u64,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("location", &self.location)
            .finish_non_exhaustive()
    }
}

impl Runtime {
    pub async fn new(state: Arc<dyn LocalStateStore>, clock: Arc<dyn Clock>) -> Result<Arc<Self>> {
        let location = state.location().await?;
        Ok(Arc::new(Self {
            state,
            clock,
            location,
            objects: RwLock::new(()),
            lifecycle: AsyncMutex::new(()),
            stopped: Mutex::new(BTreeSet::new()),
            branches: Mutex::new(BTreeMap::new()),
            dirty: Mutex::new(BTreeMap::new()),
        }))
    }

    pub async fn lock_branch(&self, branch: BranchId) -> OwnedMutexGuard<()> {
        let gate = {
            let mut branches = self.branches.lock();
            if let Some(gate) = branches.get(&branch).and_then(Weak::upgrade) {
                gate
            } else {
                branches.retain(|_, gate| gate.strong_count() != 0);
                let gate = Arc::new(AsyncMutex::new(()));
                branches.insert(branch, Arc::downgrade(&gate));
                gate
            }
        };
        gate.lock_owned().await
    }

    pub async fn branch(&self, id: BranchId) -> Result<Branch> {
        let mut branch = self
            .state
            .branch(id)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "branch does not exist"))?;
        if self.stopped.lock().contains(&id) {
            branch.state = BranchState::Stopped;
        }
        Ok(branch)
    }

    pub async fn authorize(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: Option<BranchId>,
        permission: Permission,
    ) -> Result<Workspace> {
        let value = self
            .state
            .workspace(workspace)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "workspace does not exist"))?;
        if let Some(branch) = branch
            && self.branch(branch).await?.workspace != workspace
        {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "branch belongs to another workspace",
            ));
        }
        value.authorize(actor, branch, permission)?;
        Ok(value)
    }

    pub(crate) fn mark_dirty(&self, branch: &Branch) {
        let now = self.clock.monotonic_ms();
        let mut dirty = self.dirty.lock();
        let value = dirty.entry(branch.id).or_insert(DirtyTime {
            first: now,
            last: now,
            sequence: branch.mutation_seq,
        });
        value.last = now;
        value.sequence = branch.mutation_seq;
    }

    pub(crate) fn clear_dirty(&self, branch: BranchId, sequence: u64) {
        let mut dirty = self.dirty.lock();
        if dirty
            .get(&branch)
            .is_some_and(|value| value.sequence <= sequence)
        {
            dirty.remove(&branch);
        }
    }
}

pub(crate) fn check_guard(branch: &Branch, guard: &BranchGuard) -> Result<()> {
    if branch.id != guard.branch || branch.authority_epoch != guard.authority_epoch {
        return Err(Error::new(
            ErrorCode::StaleAuthority,
            "branch authority changed",
        ));
    }
    if branch.generation != guard.generation
        || branch.formal_head != guard.expected_head
        || guard
            .mutation_seq
            .is_some_and(|sequence| sequence != branch.mutation_seq)
    {
        return Err(Error::new(ErrorCode::TargetMoved, "branch changed"));
    }
    Ok(())
}

pub(crate) fn increment(value: u64) -> Result<u64> {
    value
        .checked_add(1)
        .filter(|value| *value <= i64::MAX as u64)
        .ok_or_else(|| Error::new(ErrorCode::CapacityExceeded, "persistent counter exhausted"))
}
