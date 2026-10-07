use crate::{FileService, OperationService, RevisionService, Runtime, runtime::check_guard};
use agentfs_model::*;
use agentfs_ports::*;
use async_trait::async_trait;
use std::sync::Arc;

pub struct MountService {
    runtime: Arc<Runtime>,
    revisions: Arc<RevisionService>,
    operations: Arc<OperationService>,
    files: Arc<FileService>,
    driver: Arc<dyn MountDriver>,
    identity: LocalIdentity,
}

impl std::fmt::Debug for MountService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MountService")
            .field("driver", &self.driver.capability())
            .finish_non_exhaustive()
    }
}

impl MountService {
    pub fn new(
        runtime: Arc<Runtime>,
        revisions: Arc<RevisionService>,
        operations: Arc<OperationService>,
        files: Arc<FileService>,
        driver: Arc<dyn MountDriver>,
        identity: LocalIdentity,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime,
            revisions,
            operations,
            files,
            driver,
            identity,
        })
    }

    pub(crate) async fn plan(
        &self,
        actor: &Principal,
        request: &PrepareMount,
        generation: u64,
    ) -> Result<(Option<MountBinding>, MountBinding)> {
        let path = self.driver.normalize_path(&request.path).await?;
        let mounts = self.runtime.state.mounts().await?;
        let old = mounts
            .iter()
            .find(|mount| mount.path == path && mount.state != MountState::Released)
            .cloned();
        if let Some(old) = &old {
            self.runtime
                .authorize(actor, old.workspace, Some(old.branch), Permission::Manage)
                .await?;
        }
        if request.access == AccessMode::ReadWrite
            && mounts.iter().any(|mount| {
                mount.branch == request.branch
                    && mount.access == AccessMode::ReadWrite
                    && mount.state != MountState::Released
                    && old.as_ref().is_none_or(|old| old.id != mount.id)
            })
        {
            return Err(Error::new(
                ErrorCode::MountBusy,
                "branch already has a writable mount",
            ));
        }
        let new = MountBinding {
            id: MountId::new(),
            workspace: request.workspace,
            branch: request.branch,
            revision: request.revision,
            generation,
            path,
            principal: actor.clone(),
            identity: self.identity,
            durability: request.durability,
            access: request.access,
            state: MountState::Preparing,
        };
        Ok((old, new))
    }

    pub(crate) async fn attach_session(
        &self,
        mut operation: OperationRecord,
        old: Option<MountBinding>,
        new: Option<MountBinding>,
        session: SessionBinding,
    ) -> Result<OperationRecord> {
        operation.pending = Some(PendingAction::MountChange {
            old: old.clone(),
            new,
            session: Some(Box::new(session)),
        });
        operation.mount_ready = Some(false);
        operation.phase = if old.is_some() {
            OperationPhase::WaitingForQuiesce
        } else {
            OperationPhase::CommittedMountPending
        };
        self.runtime
            .state
            .commit(LocalCommit {
                operations: vec![operation.clone()],
                ..Default::default()
            })
            .await?;
        if old.is_some() {
            Ok(operation)
        } else {
            self.execute(operation).await
        }
    }

    async fn pause_old(&self, old: &MountBinding) -> Result<()> {
        let _gate = self.runtime.lock_branch(old.branch).await;
        let mut current = self
            .runtime
            .state
            .mount(old.id)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::StaleBinding, "old mount is missing"))?;
        if current.generation != old.generation {
            return Err(Error::new(
                ErrorCode::StaleBinding,
                "old mount generation changed",
            ));
        }
        if self.files.open_handles(old.id) != 0 {
            return Err(Error::new(
                ErrorCode::MountBusy,
                "close open file handles before releasing the mount",
            ));
        }
        if current.state != MountState::Released {
            current.state = MountState::Paused;
            self.runtime
                .state
                .commit(LocalCommit {
                    mounts: vec![current],
                    ..Default::default()
                })
                .await?;
        }
        Ok(())
    }

    async fn remove_old(&self, old: &MountBinding) -> Result<()> {
        self.pause_old(old).await?;
        let result = async {
            if old.access == AccessMode::ReadWrite {
                let branch = self.runtime.branch(old.branch).await?;
                if branch.state == BranchState::Writable
                    && branch.owner == self.runtime.location
                    && branch.mutation_seq != branch.saved_seq
                {
                    self.revisions
                        .flush(old.branch, old.principal.clone(), old.durability)
                        .await?;
                }
            }
            if self.driver.status(old).await? != NativeMountState::Absent {
                self.driver.unmount(old).await?;
            }
            let mut released = old.clone();
            released.state = MountState::Released;
            self.runtime
                .state
                .commit(LocalCommit {
                    mounts: vec![released],
                    ..Default::default()
                })
                .await
        }
        .await;
        if result.is_err() {
            let mut restored = old.clone();
            restored.state = if self.driver.status(old).await? == NativeMountState::Mounted {
                MountState::Ready
            } else {
                MountState::RecoveryRequired
            };
            self.runtime
                .state
                .commit(LocalCommit {
                    mounts: vec![restored],
                    ..Default::default()
                })
                .await?;
        }
        result
    }

    async fn install_new(&self, binding: &MountBinding) -> Result<MountBinding> {
        let mut ready = binding.clone();
        ready.state = MountState::Ready;
        self.runtime
            .state
            .commit(LocalCommit {
                mounts: vec![ready.clone()],
                ..Default::default()
            })
            .await?;
        match self.driver.status(&ready).await? {
            NativeMountState::Mounted => (),
            NativeMountState::Absent => {
                if let Err(error) = self
                    .driver
                    .mount(
                        MountSpec {
                            binding: ready.clone(),
                        },
                        self.files.clone(),
                    )
                    .await
                {
                    let mut failed = ready.clone();
                    failed.state = MountState::RecoveryRequired;
                    self.runtime
                        .state
                        .commit(LocalCommit {
                            mounts: vec![failed],
                            ..Default::default()
                        })
                        .await?;
                    return Err(error);
                }
            }
            NativeMountState::Unknown => {
                return Err(Error::new(
                    ErrorCode::MountBusy,
                    "native mount identity cannot be confirmed",
                ));
            }
        }
        Ok(ready)
    }

    async fn execute(&self, mut operation: OperationRecord) -> Result<OperationRecord> {
        let pending = operation
            .pending
            .clone()
            .ok_or_else(|| Error::invalid("operation has no pending mount action"))?;
        let result = async {
            match pending {
                PendingAction::MountChange { old, new, session } => {
                    if let Some(old) = &old {
                        self.remove_old(old).await?;
                    }
                    let binding = match new {
                        Some(new) => Some(self.install_new(&new).await?),
                        None => None,
                    };
                    let mut commit = LocalCommit::default();
                    if let Some(mut session) = session {
                        session.mount = binding.as_ref().map(|binding| binding.id);
                        operation.result = Some(CommandResult::Session {
                            binding: (*session).clone(),
                        });
                        commit.sessions.push(*session);
                    } else if let Some(binding) = binding {
                        operation.result = Some(CommandResult::Mount { binding });
                    } else {
                        operation.result = Some(CommandResult::Updated);
                    }
                    operation.phase = OperationPhase::Complete;
                    operation.pending = None;
                    operation.local_saved = true;
                    operation.mount_ready = Some(true);
                    operation.error = None;
                    commit.operations.push(operation.clone());
                    self.runtime.state.commit(commit).await?;
                    Ok(operation.clone())
                }
                PendingAction::ApplyRevision {
                    target,
                    revision,
                    merge,
                    mount,
                    durability,
                } => {
                    if !operation.local_saved {
                        if let Some(old) = &mount {
                            self.remove_old(old).await?;
                        }
                        operation = self
                            .apply_replacement(operation.clone(), target, revision, merge)
                            .await?;
                        operation = self
                            .revisions
                            .finish_durability(operation.clone(), durability)
                            .await?;
                    }
                    if let Some(old) = mount {
                        let branch = self.runtime.branch(target.branch).await?;
                        let mut new = old;
                        new.generation = branch.generation;
                        new.state = MountState::Preparing;
                        self.install_new(&new).await?;
                    }
                    operation.pending = None;
                    operation.mount_ready = Some(true);
                    operation.phase = if operation.remote_confirmed {
                        OperationPhase::Complete
                    } else {
                        OperationPhase::LocalSaved
                    };
                    let mut commit = LocalCommit {
                        operations: vec![operation.clone()],
                        ..Default::default()
                    };
                    if let Some(id) = merge {
                        let mut candidate = self
                            .runtime
                            .state
                            .merge(id)
                            .await?
                            .ok_or_else(|| Error::integrity("merge candidate is missing"))?;
                        if let Some(CommandResult::Revision { revision, .. }) = operation.result {
                            candidate.applied_revision = Some(revision);
                        }
                        commit.merges.push(candidate);
                    }
                    self.runtime.state.commit(commit).await?;
                    Ok(operation.clone())
                }
                PendingAction::Transfer { .. } => {
                    Err(Error::invalid("ownership transfer is not a mount action"))
                }
            }
        }
        .await;
        match result {
            Ok(record) => Ok(record),
            Err(error) => {
                let mut current = self
                    .runtime
                    .state
                    .operation(operation.id)
                    .await?
                    .ok_or_else(|| Error::integrity("mount operation is missing"))?;
                if !current.local_saved
                    && let Some(PendingAction::ApplyRevision {
                        mount: Some(old), ..
                    }) = &current.pending
                {
                    let branch = self.runtime.branch(old.branch).await?;
                    if branch.generation == old.generation
                        && branch.state == BranchState::Writable
                        && let Err(restore_error) = self.install_new(old).await
                    {
                        tracing::warn!(error = %restore_error, "previous mount requires recovery");
                    }
                }
                current.error = Some(error);
                current.mount_ready = Some(false);
                current.phase = if current.local_saved {
                    OperationPhase::CommittedMountPending
                } else {
                    OperationPhase::WaitingForQuiesce
                };
                self.runtime
                    .state
                    .commit(LocalCommit {
                        operations: vec![current.clone()],
                        ..Default::default()
                    })
                    .await?;
                Ok(current)
            }
        }
    }

    async fn apply_replacement(
        &self,
        operation: OperationRecord,
        target: BranchGuard,
        revision: RevisionId,
        merge: Option<MergeId>,
    ) -> Result<OperationRecord> {
        let _gate = self.runtime.lock_branch(target.branch).await;
        let _protection = self.runtime.objects.read().await;
        let branch = self.runtime.branch(target.branch).await?;
        check_guard(&branch, &target)?;
        if merge.is_none() {
            self.revisions
                .preserve_working_locked(&branch, &operation.principal)
                .await?;
        }
        let revision = self
            .revisions
            .trees
            .revision(branch.workspace, revision)
            .await?;
        self.files.replace(branch.id, &revision.root).await?;
        let parents =
            if let Some(id) = merge {
                let candidate = self.runtime.state.merge(id).await?.ok_or_else(|| {
                    Error::new(ErrorCode::NotFound, "merge candidate does not exist")
                })?;
                if candidate.revision != revision.id || candidate.target != target {
                    return Err(Error::new(
                        ErrorCode::CandidateMoved,
                        "merge candidate changed",
                    ));
                }
                if branch.dirty {
                    return Err(Error::new(
                        ErrorCode::DirtyWorktree,
                        "merge target has uncommitted changes",
                    ));
                }
                if !candidate.conflicts.is_empty() {
                    return Err(Error::new(
                        ErrorCode::UnresolvedConflict,
                        "merge has unresolved conflicts",
                    ));
                }
                if candidate.validation.as_ref().is_none_or(|validation| {
                    validation.candidate != revision.id || !validation.passed
                }) {
                    return Err(Error::new(
                        ErrorCode::ValidationFailed,
                        "current merge candidate has not passed validation",
                    ));
                }
                Some(vec![target.expected_head, candidate.source])
            } else {
                None
            };
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
                "branch has an active turn",
            ));
        }
        self.revisions
            .save_locked(
                OperationContext { operation },
                SaveRevision {
                    guard: target,
                    kind: RevisionKind::Commit,
                    parents,
                    replace_root: Some(revision.root),
                    turn: None,
                    session: None,
                    result: SaveResult::Revision,
                    durability: Durability::Local,
                },
            )
            .await
    }
}

#[async_trait]
impl MountApi for MountService {
    async fn prepare(
        &self,
        context: RequestContext,
        request: PrepareMount,
    ) -> Result<OperationRecord> {
        let _lifecycle = self.runtime.lifecycle.lock().await;
        let permission = if request.access == AccessMode::ReadWrite {
            Permission::Write
        } else {
            Permission::Read
        };
        self.runtime
            .authorize(
                &context.principal,
                request.workspace,
                Some(request.branch),
                permission,
            )
            .await?;
        let branch = self.runtime.branch(request.branch).await?;
        if request.access == AccessMode::ReadWrite {
            if request.revision.is_some() {
                return Err(Error::invalid("revision mounts must be read-only"));
            }
            branch.check_write(self.runtime.location, branch.authority_epoch)?;
        }
        if let Some(revision) = request.revision {
            self.revisions
                .trees
                .revision(request.workspace, revision)
                .await?;
        }
        let operation = match self
            .operations
            .start(
                context,
                "mount_prepare",
                &request,
                Some(request.workspace),
                Some(request.branch),
            )
            .await?
        {
            OperationStart::Existing(record) => return Ok(record),
            OperationStart::New(context) => context,
        };
        let result = async {
            let (old, new) = self
                .plan(&operation.operation.principal, &request, branch.generation)
                .await?;
            let mut record = operation.operation.clone();
            record.pending = Some(PendingAction::MountChange {
                old: old.clone(),
                new: Some(new),
                session: None,
            });
            record.mount_ready = Some(false);
            record.phase = if old.is_some() {
                OperationPhase::WaitingForQuiesce
            } else {
                OperationPhase::Saving
            };
            self.runtime
                .state
                .commit(LocalCommit {
                    operations: vec![record.clone()],
                    ..Default::default()
                })
                .await?;
            if old.is_some() {
                Ok(record)
            } else {
                self.execute(record).await
            }
        }
        .await;
        match result {
            Ok(record) => Ok(record),
            Err(error) => self.operations.fail(&operation, error).await,
        }
    }

    async fn release(
        &self,
        context: RequestContext,
        request: ReleaseMount,
    ) -> Result<OperationRecord> {
        let _lifecycle = self.runtime.lifecycle.lock().await;
        let target = self
            .operations
            .get(&context.principal, request.operation)
            .await?;
        let operation = match self
            .operations
            .start(
                context,
                "mount_release",
                &request,
                target.workspace,
                target.branch,
            )
            .await?
        {
            OperationStart::Existing(record) => return Ok(record),
            OperationStart::New(context) => context,
        };
        let result = async {
            let old = match &target.pending {
                Some(PendingAction::MountChange { old, .. }) => old.as_ref(),
                Some(PendingAction::ApplyRevision { mount, .. }) => mount.as_ref(),
                Some(PendingAction::Transfer { .. }) => {
                    return Err(Error::invalid("operation is an ownership transfer"));
                }
                None => {
                    return self
                        .operations
                        .finish(
                            operation.clone(),
                            CommandResult::Operation {
                                operation: target.id,
                            },
                            LocalCommit::default(),
                        )
                        .await;
                }
            };
            if old.is_some_and(|old| old.generation != request.expected_binding_generation) {
                return Err(Error::new(
                    ErrorCode::StaleBinding,
                    "release acknowledged a different binding generation",
                ));
            }
            let resumed = self.execute(target.clone()).await?;
            if let Some(error) = resumed.error {
                return Err(error);
            }
            self.operations
                .finish(
                    operation.clone(),
                    CommandResult::Operation {
                        operation: target.id,
                    },
                    LocalCommit::default(),
                )
                .await
        }
        .await;
        match result {
            Ok(record) => Ok(record),
            Err(error) => self.operations.fail(&operation, error).await,
        }
    }

    async fn unmount(
        &self,
        context: RequestContext,
        id: MountId,
        generation: u64,
    ) -> Result<OperationRecord> {
        let _lifecycle = self.runtime.lifecycle.lock().await;
        let mount = self
            .runtime
            .state
            .mount(id)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "mount does not exist"))?;
        self.runtime
            .authorize(
                &context.principal,
                mount.workspace,
                Some(mount.branch),
                Permission::Manage,
            )
            .await?;
        if mount.generation != generation {
            return Err(Error::new(
                ErrorCode::StaleBinding,
                "mount generation changed",
            ));
        }
        let operation = match self
            .operations
            .start(
                context,
                "mount_unmount",
                &(id, generation),
                Some(mount.workspace),
                Some(mount.branch),
            )
            .await?
        {
            OperationStart::Existing(record) => return Ok(record),
            OperationStart::New(context) => context,
        };
        let mut record = operation.operation.clone();
        record.pending = Some(PendingAction::MountChange {
            old: Some(mount),
            new: None,
            session: None,
        });
        record.phase = OperationPhase::WaitingForQuiesce;
        record.mount_ready = Some(false);
        self.runtime
            .state
            .commit(LocalCommit {
                operations: vec![record.clone()],
                ..Default::default()
            })
            .await?;
        self.execute(record).await
    }
}

#[async_trait]
impl RevisionTransition for MountService {
    async fn replace_revision(
        &self,
        context: OperationContext,
        target: BranchGuard,
        revision: RevisionId,
        merge: Option<MergeId>,
        durability: Durability,
    ) -> Result<OperationRecord> {
        let _lifecycle = self.runtime.lifecycle.lock().await;
        let record = {
            let _gate = self.runtime.lock_branch(target.branch).await;
            let branch = self.runtime.branch(target.branch).await?;
            check_guard(&branch, &target)?;
            self.revisions
                .trees
                .revision(branch.workspace, revision)
                .await?;
            if merge.is_none() {
                branch.check_write(self.runtime.location, target.authority_epoch)?;
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
                    "branch has an active turn",
                ));
            }
            let mount = self
                .runtime
                .state
                .mounts()
                .await?
                .into_iter()
                .find(|mount| {
                    mount.branch == target.branch
                        && mount.access == AccessMode::ReadWrite
                        && mount.state != MountState::Released
                });
            let mut record = context.operation;
            record.pending = Some(PendingAction::ApplyRevision {
                target,
                revision,
                merge,
                mount: mount.clone(),
                durability,
            });
            record.phase = if mount.is_some() {
                OperationPhase::WaitingForQuiesce
            } else {
                OperationPhase::Saving
            };
            record.mount_ready = Some(false);
            self.runtime
                .state
                .commit(LocalCommit {
                    operations: vec![record.clone()],
                    ..Default::default()
                })
                .await?;
            record
        };
        if matches!(
            &record.pending,
            Some(PendingAction::ApplyRevision { mount: Some(_), .. })
        ) {
            Ok(record)
        } else {
            self.execute(record).await
        }
    }
}
