use agentfs_model::*;
use agentfs_ports::*;
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
    },
    service::RequestContext as McpContext,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct McpServer {
    services: Services,
    principal: Principal,
}
impl McpServer {
    pub fn new(services: Services, principal: Principal) -> Self {
        Self {
            services,
            principal,
        }
    }

    async fn dispatch(&self, name: &str, arguments: Value) -> Result<Value> {
        macro_rules! command {
            ($request:ty, $api:ident, $method:ident) => {{
                let input: Command<$request> = parse(arguments)?;
                let context = RequestContext {
                    principal: self.principal.clone(),
                    request_id: input.request_id,
                };
                context.validate()?;
                encode(self.services.$api.$method(context, input.request).await?)
            }};
        }
        match name {
            "workspace_create" => command!(CreateWorkspace, workspace, create),
            "workspace_import" => command!(ImportDirectory, workspace, import_directory),
            "workspace_export" => command!(ExportRevision, workspace, export_revision),
            "revision_commit" => command!(CommitRevision, revisions, commit),
            "branch_fork" => command!(ForkBranch, revisions, fork),
            "branch_restore" => command!(RestoreBranch, revisions, restore),
            "session_open" => command!(OpenSession, sessions, open),
            "session_resume" => command!(ResumeSession, sessions, resume),
            "session_pause" => command!(SessionAction, sessions, pause),
            "session_close" => command!(SessionAction, sessions, close),
            "turn_begin" => command!(BeginTurn, sessions, turn_begin),
            "turn_end" => command!(EndTurn, sessions, turn_end),
            "sync" => command!(SyncRequest, replica, sync),
            "cache_prefetch" => command!(RevisionRange, cache, prefetch),
            "cache_pin" => command!(PinRequest, cache, pin),
            "ownership_transfer" => command!(TransferOwnership, ownership, transfer),
            "merge_prepare" => command!(PrepareMerge, merge, prepare),
            "merge_resolve" => command!(ResolveMerge, merge, resolve),
            "merge_validate" => command!(ValidateMerge, merge, validate),
            "merge_apply" => command!(ApplyMerge, merge, apply),
            "mount_prepare" => command!(PrepareMount, mounts, prepare),
            "mount_release" => command!(ReleaseMount, mounts, release),
            "workspace_list" => {
                let _: Empty = parse(arguments)?;
                encode(self.services.workspace.list(&self.principal).await?)
            }
            "workspace_status" => {
                let input: WorkspaceQuery = parse(arguments)?;
                encode(
                    self.services
                        .workspace
                        .status(&self.principal, input.workspace)
                        .await?,
                )
            }
            "revision_list" => {
                let input: RevisionList = parse(arguments)?;
                encode(
                    self.services
                        .revisions
                        .list(&self.principal, input.workspace, input.branch)
                        .await?,
                )
            }
            "revision_diff" => {
                let input: DiffQuery = parse(arguments)?;
                encode(
                    self.services
                        .revisions
                        .diff(&self.principal, input.workspace, input.before, input.after)
                        .await?,
                )
            }
            "revision_read" => {
                let input: ReadQuery = parse(arguments)?;
                let bytes = self
                    .services
                    .revisions
                    .read_file(
                        &self.principal,
                        input.workspace,
                        input.revision,
                        input.path,
                        input.offset,
                        input.size,
                    )
                    .await?;
                Ok(json!({"bytes": bytes.to_vec(), "length": bytes.len()}))
            }
            "cache_status" => encode(
                self.services
                    .cache
                    .status(&self.principal, parse(arguments)?)
                    .await?,
            ),
            "merge_get" => {
                let input: MergeQuery = parse(arguments)?;
                encode(
                    self.services
                        .merge
                        .get(&self.principal, input.merge)
                        .await?,
                )
            }
            "operation_get" => {
                let input: OperationQuery = parse(arguments)?;
                encode(
                    self.services
                        .operations
                        .get(&self.principal, input.operation)
                        .await?,
                )
            }
            "operation_by_request" => {
                let input: RequestQuery = parse(arguments)?;
                encode(
                    self.services
                        .operations
                        .by_request(&self.principal, &input.request_id)
                        .await?,
                )
            }
            "operation_cancel" => {
                let input: OperationQuery = parse(arguments)?;
                encode(
                    self.services
                        .operations
                        .cancel(&self.principal, input.operation)
                        .await?,
                )
            }
            "mount_unmount" => {
                let input: Command<Unmount> = parse(arguments)?;
                let context = RequestContext {
                    principal: self.principal.clone(),
                    request_id: input.request_id,
                };
                context.validate()?;
                encode(
                    self.services
                        .mounts
                        .unmount(context, input.request.mount, input.request.generation)
                        .await?,
                )
            }
            _ => Err(Error::new(ErrorCode::NotFound, "unknown tool")),
        }
    }
}
fn parse<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|error| Error::invalid(error.to_string()))
}
fn encode(value: impl Serialize) -> Result<Value> {
    serde_json::to_value(value).map_err(|error| Error::integrity(error.to_string()))
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(Implementation::new("agentfs", env!("CARGO_PKG_VERSION"))).with_instructions("AgentFS manages durable workspaces and isolated agent sessions. Mutation tools require a stable request_id. Retry with the same request_id and identical request. Inspect local_saved, remote_confirmed, mount_ready and phase independently. A waiting_for_quiesce operation requires the caller to stop processes using the mount before mount_release. Use workspace_status to obtain current BranchGuard values.")
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: McpContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: tool_catalog(),
            ..Default::default()
        })
    }
    fn get_tool(&self, name: &str) -> Option<Tool> {
        tool_catalog().into_iter().find(|tool| tool.name == name)
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: McpContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        let result = self
            .dispatch(
                &request.name,
                Value::Object(request.arguments.unwrap_or_default()),
            )
            .await;
        let (value, is_error) = match result {
            Ok(value) => {
                let failed = value.get("error").is_some_and(|value| !value.is_null());
                (value, failed)
            }
            Err(error) => (json!({"error": error}), true),
        };
        Ok(if is_error {
            CallToolResult::structured_error(value)
        } else {
            CallToolResult::structured(value)
        }
        .into())
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Command<T> {
    request_id: String,
    request: T,
}
macro_rules! query {
    ($name:ident { $($field:ident : $kind:ty),* $(,)? }) => {
        #[derive(Debug, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        struct $name { $($field: $kind),* }
    };
}
query!(Empty {});
query!(WorkspaceQuery {
    workspace: WorkspaceId
});
query!(RevisionList { workspace: WorkspaceId, branch: Option<BranchId> });
query!(DiffQuery {
    workspace: WorkspaceId,
    before: RevisionId,
    after: RevisionId
});
query!(ReadQuery {
    workspace: WorkspaceId,
    revision: RevisionId,
    path: WorkspacePath,
    offset: u64,
    size: u32
});
query!(MergeQuery { merge: MergeId });
query!(OperationQuery {
    operation: OperationId
});
query!(RequestQuery { request_id: String });
query!(Unmount {
    mount: MountId,
    generation: u64
});

fn tool<T: JsonSchema>(name: &'static str, description: &'static str, read_only: bool) -> Tool {
    let schema = schemars::schema_for!(T)
        .as_object()
        .cloned()
        .unwrap_or_default();
    let mut tool = Tool::new(name, description, Arc::new(schema));
    tool.annotations = Some(ToolAnnotations::from_raw(
        None,
        Some(read_only),
        Some(!read_only),
        Some(true),
        Some(true),
    ));
    tool
}
pub fn tool_catalog() -> Vec<Tool> {
    vec![
        tool::<Command<CreateWorkspace>>(
            "workspace_create",
            "Create a workspace and its protected main branch.",
            false,
        ),
        tool::<Empty>(
            "workspace_list",
            "List workspaces visible to this authenticated principal.",
            true,
        ),
        tool::<WorkspaceQuery>(
            "workspace_status",
            "Inspect branches, mounts, location, pending synchronization and capabilities.",
            true,
        ),
        tool::<Command<ImportDirectory>>(
            "workspace_import",
            "Import a stable host directory from a configured import root into a new branch.",
            false,
        ),
        tool::<Command<ExportRevision>>(
            "workspace_export",
            "Export a revision to a new directory within a configured export root.",
            false,
        ),
        tool::<Command<CommitRevision>>(
            "revision_commit",
            "Save a formal commit or checkpoint using a current branch guard.",
            false,
        ),
        tool::<RevisionList>(
            "revision_list",
            "List retained revisions, optionally filtered by branch.",
            true,
        ),
        tool::<DiffQuery>(
            "revision_diff",
            "Compare file contents and supported attributes between revisions.",
            true,
        ),
        tool::<ReadQuery>(
            "revision_read",
            "Read up to 4 MiB of an immutable file as a byte array.",
            true,
        ),
        tool::<Command<ForkBranch>>(
            "branch_fork",
            "Create an isolated writable branch from a retained revision.",
            false,
        ),
        tool::<Command<RestoreBranch>>(
            "branch_restore",
            "Restore a branch to a revision with an explicit mount handoff when occupied.",
            false,
        ),
        tool::<Command<OpenSession>>(
            "session_open",
            "Create an isolated agent session and optionally mount its workspace directory.",
            false,
        ),
        tool::<Command<ResumeSession>>(
            "session_resume",
            "Resume saved working state; interrupted turns require a recovery decision.",
            false,
        ),
        tool::<Command<SessionAction>>(
            "session_pause",
            "Save and pause a session, releasing its mount after caller quiescence.",
            false,
        ),
        tool::<Command<SessionAction>>(
            "session_close",
            "Save and close a session, releasing its mount after caller quiescence.",
            false,
        ),
        tool::<Command<BeginTurn>>(
            "turn_begin",
            "Persist an agent turn identifier before work begins.",
            false,
        ),
        tool::<Command<EndTurn>>(
            "turn_end",
            "Atomically save working changes and the immutable completed, failed or cancelled turn result.",
            false,
        ),
        tool::<Command<SyncRequest>>(
            "sync",
            "Push pending immutable objects and branch references, pull remote history, or both.",
            false,
        ),
        tool::<Command<RevisionRange>>(
            "cache_prefetch",
            "Fetch requested revision files for local access.",
            false,
        ),
        tool::<Command<PinRequest>>(
            "cache_pin",
            "Pin or unpin revision content against local eviction.",
            false,
        ),
        tool::<RevisionRange>(
            "cache_status",
            "Inspect local completeness and pin status of a revision range.",
            true,
        ),
        tool::<Command<TransferOwnership>>(
            "ownership_transfer",
            "Stop local writes, synchronize, and transfer branch authority to another location.",
            false,
        ),
        tool::<Command<PrepareMerge>>(
            "merge_prepare",
            "Create a three-way merge candidate with explicit file conflicts.",
            false,
        ),
        tool::<MergeQuery>(
            "merge_get",
            "Inspect a merge candidate, conflicts and validation.",
            true,
        ),
        tool::<Command<ResolveMerge>>(
            "merge_resolve",
            "Resolve conflicts and create a new candidate, invalidating previous validation.",
            false,
        ),
        tool::<Command<ValidateMerge>>(
            "merge_validate",
            "Validate candidate integrity and optionally run an administrator-configured container check.",
            false,
        ),
        tool::<Command<ApplyMerge>>(
            "merge_apply",
            "Apply a validated candidate with two-parent history and target generation checks.",
            false,
        ),
        tool::<Command<PrepareMount>>(
            "mount_prepare",
            "Prepare a read-only revision or writable branch mount within configured mount roots.",
            false,
        ),
        tool::<Command<ReleaseMount>>(
            "mount_release",
            "Continue a pending mount handoff after all processes release the expected mount generation.",
            false,
        ),
        tool::<Command<Unmount>>(
            "mount_unmount",
            "Unmount an idle binding with generation verification.",
            false,
        ),
        tool::<OperationQuery>(
            "operation_get",
            "Inspect a durable operation result and independent durability flags.",
            true,
        ),
        tool::<RequestQuery>(
            "operation_by_request",
            "Recover a lost response using its original request_id.",
            true,
        ),
        tool::<OperationQuery>(
            "operation_cancel",
            "Cancel an operation that has not committed its effects.",
            false,
        ),
    ]
}
