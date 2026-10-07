use crate::{
    OperationService, ReplicaService, RevisionService, Runtime,
    replica::decode_branch_ref,
    runtime::{check_guard, increment},
};
use agentfs_model::*;
use agentfs_ports::*;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug)]
pub struct OwnershipService {
    runtime: Arc<Runtime>,
    revisions: Arc<RevisionService>,
    replica: Arc<ReplicaService>,
    operations: Arc<OperationService>,
    transfers: Mutex<()>,
}

impl OwnershipService {
    pub fn new(
        runtime: Arc<Runtime>,
        revisions: Arc<RevisionService>,
        replica: Arc<ReplicaService>,
        operations: Arc<OperationService>,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime,
            revisions,
            replica,
            operations,
            transfers: Mutex::new(()),
        })
    }

    async fn perform(
        &self,
        mut operation: OperationRecord,
        request: TransferOwnership,
    ) -> Result<OperationRecord> {
        let content = &self.revisions.trees.content;
        let refs = content.refs.as_ref().ok_or_else(|| {
            Error::new(
                ErrorCode::RemoteRequired,
                "ownership transfer requires remote storage",
            )
        })?;
        let branch = {
            let _gate = self.runtime.lock_branch(request.guard.branch).await;
            let _protection = self.runtime.objects.read().await;
            let mut branch = self.runtime.branch(request.guard.branch).await?;
            check_guard(&branch, &request.guard)?;
            if branch.owner != self.runtime.location {
                return Err(Error::new(
                    ErrorCode::StaleAuthority,
                    "this location does not own the branch",
                ));
            }
            if branch.state == BranchState::Deleted {
                return Err(Error::new(
                    ErrorCode::ReadOnly,
                    "deleted branch cannot be transferred",
                ));
            }
            if self.runtime.state.mounts().await?.iter().any(|mount| {
                mount.branch == branch.id
                    && mount.access == AccessMode::ReadWrite
                    && mount.state != MountState::Released
            }) {
                return Err(Error::new(
                    ErrorCode::MountBusy,
                    "release the writable mount before transferring ownership",
                ));
            }
            if self
                .runtime
                .state
                .sessions(branch.workspace)
                .await?
                .iter()
                .any(|session| session.branch == branch.id && session.active_turn.is_some())
            {
                return Err(Error::new(
                    ErrorCode::BranchBusy,
                    "finish the active turn before transferring ownership",
                ));
            }
            if operation.pending.is_none() {
                if branch.state != BranchState::Writable {
                    return Err(Error::new(
                        ErrorCode::RecoveryRequired,
                        "stopped branch requires recovery of its original transfer",
                    ));
                }
                branch = self
                    .revisions
                    .preserve_working_locked(&branch, &operation.principal)
                    .await?;
                branch.state = BranchState::Stopped;
                operation.pending = Some(PendingAction::Transfer {
                    target: request.target,
                    guard: request.guard,
                });
                operation.phase = OperationPhase::Saving;
                self.runtime
                    .state
                    .commit(LocalCommit {
                        guards: vec![request.guard],
                        branches: vec![branch.clone()],
                        operations: vec![operation.clone()],
                        ..Default::default()
                    })
                    .await?;
            }
            branch
        };
        self.replica
            .drain(branch.workspace, Some(branch.id))
            .await?;
        let _protection = self.runtime.objects.read().await;
        let mut next = branch.clone();
        next.owner = request.target;
        next.authority_epoch = increment(branch.authority_epoch)?;
        next.generation = increment(branch.generation)?;
        next.source_generation = increment(branch.source_generation)?;
        next.state = BranchState::Writable;
        let mut result = operation.clone();
        result.pending = None;
        result.local_saved = true;
        result.remote_confirmed = true;
        result.phase = OperationPhase::Complete;
        result.error = None;
        result.result = Some(CommandResult::Ownership {
            branch: branch.id,
            owner: next.owner,
            authority_epoch: next.authority_epoch,
        });
        let job = self.replica.build_job(&next, None, &result, None).await?;
        content
            .register_publication(
                branch.workspace,
                operation.id,
                vec![
                    job.reference.working_root.clone(),
                    job.reference.record_index.clone(),
                ],
            )
            .await?;
        let key = RefKey::Branch {
            workspace: branch.workspace,
            branch: branch.id,
        };
        let mut uploaded = false;
        for _ in 0..8 {
            let current = refs.get(&key).await?.ok_or_else(|| {
                Error::new(
                    ErrorCode::StaleAuthority,
                    "remote authority record is missing",
                )
            })?;
            let remote = decode_branch_ref(branch.workspace, branch.id, &current.bytes)?;
            if self.replica.contains_operation(&remote, &result).await? {
                next.state = BranchState::Stopped;
                self.runtime
                    .state
                    .commit(LocalCommit {
                        guards: vec![request.guard],
                        branches: vec![next],
                        operations: vec![result.clone()],
                        remote_refs: vec![remote],
                        ..Default::default()
                    })
                    .await?;
                content
                    .release_remote(branch.workspace, operation.id)
                    .await?;
                return Ok(result);
            }
            if remote.owner != branch.owner
                || remote.authority_epoch != branch.authority_epoch
                || remote.source_generation != branch.source_generation
            {
                return Err(Error::new(
                    ErrorCode::StaleAuthority,
                    "remote branch changed before ownership transfer",
                ));
            }
            if !uploaded {
                self.replica
                    .upload_closure(
                        branch.workspace,
                        vec![
                            job.reference.record_index.clone(),
                            job.reference.working_root.clone(),
                        ],
                    )
                    .await?;
                uploaded = true;
            }
            match refs
                .compare_exchange(RefUpdate {
                    key: key.clone(),
                    expected_version: Some(current.version),
                    bytes: encode(&job.reference)?.into(),
                })
                .await?
            {
                CasOutcome::Applied { .. } => {
                    next.state = BranchState::Stopped;
                    self.runtime
                        .state
                        .commit(LocalCommit {
                            guards: vec![request.guard],
                            branches: vec![next],
                            operations: vec![result.clone()],
                            remote_refs: vec![job.reference.clone()],
                            ..Default::default()
                        })
                        .await?;
                    content
                        .release_remote(branch.workspace, operation.id)
                        .await?;
                    return Ok(result);
                }
                CasOutcome::Conflict | CasOutcome::Unknown => continue,
            }
        }
        Err(Error::new(
            ErrorCode::RemoteUnknown,
            "ownership transfer remains unconfirmed",
        )
        .retryable())
    }
}

#[async_trait]
impl OwnershipApi for OwnershipService {
    async fn transfer(
        &self,
        context: RequestContext,
        request: TransferOwnership,
    ) -> Result<OperationRecord> {
        let _transfer = self.transfers.lock().await;
        let _lifecycle = self.runtime.lifecycle.lock().await;
        let branch = self.runtime.branch(request.guard.branch).await?;
        self.runtime
            .authorize(
                &context.principal,
                branch.workspace,
                Some(branch.id),
                Permission::Manage,
            )
            .await?;
        if request.target == self.runtime.location {
            return Err(Error::invalid("ownership target is already this location"));
        }
        let operation = match self
            .operations
            .start(
                context,
                "branch_transfer",
                &request,
                Some(branch.workspace),
                Some(branch.id),
            )
            .await?
        {
            OperationStart::Existing(record)
                if matches!(record.pending, Some(PendingAction::Transfer { .. })) =>
            {
                record
            }
            OperationStart::Existing(record) => return Ok(record),
            OperationStart::New(context) => context.operation,
        };
        match self.perform(operation.clone(), request).await {
            Ok(record) => Ok(record),
            Err(error) => {
                let mut record = self
                    .runtime
                    .state
                    .operation(operation.id)
                    .await?
                    .ok_or_else(|| Error::integrity("ownership operation is missing"))?;
                if record.remote_confirmed {
                    return Ok(record);
                }
                record.phase = if record.pending.is_some() {
                    OperationPhase::RecoveryRequired
                } else {
                    OperationPhase::Failed
                };
                record.error = Some(error);
                self.runtime
                    .state
                    .commit(LocalCommit {
                        operations: vec![record.clone()],
                        ..Default::default()
                    })
                    .await?;
                Ok(record)
            }
        }
    }
}
