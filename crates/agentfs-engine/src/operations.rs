use crate::Runtime;
use agentfs_model::*;
use agentfs_ports::*;
use async_trait::async_trait;
use serde::Serialize;
use std::sync::Arc;

#[derive(Debug)]
pub struct OperationService {
    runtime: Arc<Runtime>,
}

impl OperationService {
    pub fn new(runtime: Arc<Runtime>) -> Arc<Self> {
        Arc::new(Self { runtime })
    }

    pub async fn start<T: Serialize + Sync>(
        &self,
        context: RequestContext,
        name: &str,
        request: &T,
        workspace: Option<WorkspaceId>,
        branch: Option<BranchId>,
    ) -> Result<OperationStart> {
        let fingerprint = object_ref(ObjectKind::RecordIndex, &encode(&(name, request))?).id;
        self.begin(context, name, fingerprint, workspace, branch)
            .await
    }

    pub async fn finish(
        &self,
        context: OperationContext,
        result: CommandResult,
        mut commit: LocalCommit,
    ) -> Result<OperationRecord> {
        let mut operation = context.operation;
        operation.phase = OperationPhase::Complete;
        operation.local_saved = true;
        operation.result = Some(result);
        operation.error = None;
        commit.operations.push(operation.clone());
        self.runtime.state.commit(commit).await?;
        Ok(operation)
    }
}

#[async_trait]
impl OperationCoordinator for OperationService {
    async fn begin(
        &self,
        context: RequestContext,
        name: &str,
        fingerprint: ObjectId,
        workspace: Option<WorkspaceId>,
        branch: Option<BranchId>,
    ) -> Result<OperationStart> {
        context.validate()?;
        let record = OperationRecord {
            id: OperationId::new(),
            request_id: context.request_id,
            principal: context.principal,
            workspace,
            branch,
            name: name.into(),
            fingerprint,
            phase: OperationPhase::Reserved,
            local_saved: false,
            remote_confirmed: false,
            mount_ready: None,
            result: None,
            error: None,
            pending: None,
            created_ns: self.runtime.clock.now_ns(),
        };
        let reserved = self.runtime.state.reserve_operation(record).await?;
        Ok(if reserved.created {
            OperationStart::New(OperationContext {
                operation: reserved.record,
            })
        } else {
            OperationStart::Existing(reserved.record)
        })
    }

    async fn fail(&self, context: &OperationContext, error: Error) -> Result<OperationRecord> {
        let mut operation = self
            .runtime
            .state
            .operation(context.operation.id)
            .await?
            .ok_or_else(|| Error::integrity("operation reservation is missing"))?;
        if !operation.local_saved {
            operation.phase = OperationPhase::Failed;
            operation.error = Some(error);
            self.runtime
                .state
                .commit(LocalCommit {
                    operations: vec![operation.clone()],
                    ..Default::default()
                })
                .await?;
        }
        Ok(operation)
    }
}

#[async_trait]
impl OperationApi for OperationService {
    async fn get(&self, actor: &Principal, id: OperationId) -> Result<OperationRecord> {
        let operation = self
            .runtime
            .state
            .operation(id)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "operation does not exist"))?;
        if &operation.principal != actor {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "operation belongs to another principal",
            ));
        }
        Ok(operation)
    }

    async fn by_request(&self, actor: &Principal, request: &str) -> Result<OperationRecord> {
        self.runtime
            .state
            .operation_by_request(actor, request)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "request does not exist"))
    }

    async fn cancel(&self, actor: &Principal, id: OperationId) -> Result<OperationRecord> {
        let _lifecycle = self.runtime.lifecycle.lock().await;
        let mut operation = self.get(actor, id).await?;
        if operation.phase.terminal() {
            return Ok(operation);
        }
        if operation.phase != OperationPhase::WaitingForQuiesce || operation.local_saved {
            return Err(Error::new(
                ErrorCode::Busy,
                "operation cannot be cancelled at this stage",
            ));
        }
        let old = match &operation.pending {
            Some(PendingAction::MountChange { old, .. }) => old.as_ref(),
            Some(PendingAction::ApplyRevision { mount, .. }) => mount.as_ref(),
            _ => {
                return Err(Error::new(
                    ErrorCode::Busy,
                    "operation has no cancellable mount handoff",
                ));
            }
        };
        if let Some(old) = old {
            let current = self.runtime.state.mount(old.id).await?;
            if current.is_none_or(|mount| {
                mount.generation != old.generation || mount.state != MountState::Ready
            }) {
                return Err(Error::new(
                    ErrorCode::Busy,
                    "mount handoff has already changed the binding",
                ));
            }
        }
        operation.phase = OperationPhase::Cancelled;
        operation.pending = None;
        self.runtime
            .state
            .commit(LocalCommit {
                operations: vec![operation.clone()],
                ..Default::default()
            })
            .await?;
        Ok(operation)
    }
}
