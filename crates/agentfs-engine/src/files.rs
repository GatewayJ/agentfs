use crate::{Runtime, TreeService, runtime::increment};
use agentfs_model::*;
use agentfs_ports::*;
use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex;
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Debug)]
struct OpenFile {
    mount: MountId,
    generation: u64,
    branch: BranchId,
    inode: InodeId,
    mode: OpenMode,
}

pub struct FileService {
    runtime: Arc<Runtime>,
    trees: Arc<TreeService>,
    working: Arc<dyn WorkingTreeStore>,
    durability: Arc<dyn DurabilityService>,
    handles: Mutex<BTreeMap<HandleId, OpenFile>>,
}

impl std::fmt::Debug for FileService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileService")
            .field("open_handles", &self.handles.lock().len())
            .finish_non_exhaustive()
    }
}

impl FileService {
    pub fn new(
        runtime: Arc<Runtime>,
        trees: Arc<TreeService>,
        working: Arc<dyn WorkingTreeStore>,
        durability: Arc<dyn DurabilityService>,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime,
            trees,
            working,
            durability,
            handles: Mutex::new(BTreeMap::new()),
        })
    }

    async fn binding(&self, context: &FsContext, write: bool) -> Result<(MountBinding, Branch)> {
        let mount = self
            .runtime
            .state
            .mount(context.mount)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::StaleBinding, "mount binding no longer exists"))?;
        if mount.generation != context.generation || mount.state != MountState::Ready {
            return Err(Error::new(
                ErrorCode::StaleBinding,
                "mount binding is not active",
            ));
        }
        if context.identity.uid != mount.identity.uid && context.identity.uid != 0 {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "local user cannot access this mount",
            ));
        }
        let permission = if write {
            Permission::Write
        } else {
            Permission::Read
        };
        self.runtime
            .authorize(
                &mount.principal,
                mount.workspace,
                Some(mount.branch),
                permission,
            )
            .await?;
        let branch = self.runtime.branch(mount.branch).await?;
        if mount.revision.is_none() && branch.generation != mount.generation {
            return Err(Error::new(
                ErrorCode::StaleBinding,
                "branch generation changed",
            ));
        }
        if self.runtime.stopped.lock().contains(&branch.id) {
            return Err(Error::new(
                ErrorCode::RecoveryRequired,
                "branch stopped after a storage failure",
            ));
        }
        if write {
            if mount.access != AccessMode::ReadWrite || mount.revision.is_some() {
                return Err(Error::new(ErrorCode::ReadOnly, "mount is read-only"));
            }
            if branch.generation != mount.generation {
                return Err(Error::new(
                    ErrorCode::StaleBinding,
                    "branch generation changed",
                ));
            }
            branch.check_write(self.runtime.location, branch.authority_epoch)?;
        }
        Ok((mount, branch))
    }

    async fn nodes(&self, mount: &MountBinding) -> Result<Vec<Node>> {
        if let Some(id) = mount.revision {
            let revision = self.trees.revision(mount.workspace, id).await?;
            Ok(self
                .trees
                .tree(mount.workspace, &revision.root)
                .await?
                .into_iter()
                .map(|(path, inode)| Node {
                    path,
                    inode,
                    dirty: false,
                })
                .collect())
        } else {
            self.runtime.state.nodes(mount.branch).await
        }
    }

    async fn node(&self, mount: &MountBinding, id: InodeId) -> Result<Node> {
        let node = if mount.revision.is_some() {
            self.nodes(mount)
                .await?
                .into_iter()
                .find(|node| node.inode.id == id)
        } else {
            self.runtime.state.node(mount.branch, id).await?
        };
        node.ok_or_else(|| Error::new(ErrorCode::NotFound, "inode does not exist"))
    }

    fn local_attributes(mount: &MountBinding, mut inode: Inode) -> Inode {
        inode.uid = mount.identity.uid;
        inode.gid = mount.identity.gid;
        inode
    }

    fn permission(context: &FsContext, inode: &Inode, required: u32) -> Result<()> {
        if context.identity.uid == 0 || ((inode.mode >> 6) & required) == required {
            Ok(())
        } else {
            Err(Error::new(
                ErrorCode::PermissionDenied,
                "inode permissions deny access",
            ))
        }
    }

    fn directory(node: &Node) -> Result<()> {
        if node.inode.kind != FileKind::Directory {
            return Err(Error::new(
                ErrorCode::NotDirectory,
                "inode is not a directory",
            ));
        }
        Ok(())
    }

    fn working_file(branch: &Branch, node: &Node) -> WorkingFile {
        WorkingFile {
            workspace: branch.workspace,
            branch: branch.id,
            inode: node.inode.id,
        }
    }

    async fn prepare_write(&self, branch: &Branch, node: &Node) -> Result<()> {
        if !node.dirty {
            let content = node
                .inode
                .content
                .as_ref()
                .ok_or_else(|| Error::integrity("file content is missing"))?;
            let stream = self.trees.content.open(branch.workspace, content).await?;
            self.working
                .create(Self::working_file(branch, node), Some(stream))
                .await?;
        }
        Ok(())
    }

    async fn check_capacity(
        &self,
        branch: &Branch,
        nodes: &[Node],
        old_size: u64,
        new_size: u64,
        additional_nodes: u64,
    ) -> Result<()> {
        let workspace = self
            .runtime
            .state
            .workspace(branch.workspace)
            .await?
            .ok_or_else(|| Error::integrity("workspace is missing"))?;
        if new_size > workspace.max_file_bytes
            || nodes.len() as u64 + additional_nodes > workspace.max_inodes
        {
            return Err(Error::new(
                ErrorCode::CapacityExceeded,
                "workspace file or inode limit exceeded",
            ));
        }
        let total = nodes
            .iter()
            .try_fold(0u64, |total, node| total.checked_add(node.inode.size))
            .ok_or_else(|| Error::integrity("working tree size overflow"))?;
        let next = total
            .checked_sub(old_size)
            .and_then(|total| total.checked_add(new_size))
            .ok_or_else(|| Error::integrity("invalid working tree size"))?;
        if next > workspace.max_working_bytes {
            return Err(Error::new(
                ErrorCode::CapacityExceeded,
                "workspace size limit exceeded",
            ));
        }
        Ok(())
    }

    async fn commit_nodes(
        &self,
        mut branch: Branch,
        changes: Vec<NodeChange>,
        next_inode: Option<u64>,
    ) -> Result<()> {
        let guard = BranchGuard::from(&branch);
        branch.mutation_seq = increment(branch.mutation_seq)?;
        branch.dirty = true;
        if let Some(next) = next_inode {
            branch.next_inode = next;
        }
        self.runtime
            .state
            .commit(LocalCommit {
                guards: vec![guard],
                branches: vec![branch.clone()],
                node_changes: changes,
                ..Default::default()
            })
            .await?;
        self.runtime.mark_dirty(&branch);
        Ok(())
    }

    fn handle(&self, context: &FsContext, id: HandleId) -> Result<OpenFile> {
        let handle = self
            .handles
            .lock()
            .get(&id)
            .cloned()
            .ok_or_else(|| Error::new(ErrorCode::StaleBinding, "file handle is closed"))?;
        if handle.mount != context.mount || handle.generation != context.generation {
            return Err(Error::new(
                ErrorCode::StaleBinding,
                "file handle belongs to another binding",
            ));
        }
        Ok(handle)
    }

    fn inode_open(&self, branch: BranchId, inode: InodeId) -> bool {
        self.handles
            .lock()
            .values()
            .any(|handle| handle.branch == branch && handle.inode == inode)
    }

    async fn stop_after_write_error(&self, branch: &Branch, error: Error) -> Error {
        self.runtime.stopped.lock().insert(branch.id);
        let mut stopped = branch.clone();
        stopped.state = BranchState::Stopped;
        if let Err(stop_error) = self
            .runtime
            .state
            .commit(LocalCommit {
                branches: vec![stopped],
                ..Default::default()
            })
            .await
        {
            tracing::error!(branch = %branch.id, error = %stop_error, "cannot persist branch stop after failed write");
        }
        error
    }
}

#[async_trait]
impl FileSystem for FileService {
    async fn lookup(&self, context: &FsContext, parent: InodeId, name: &str) -> Result<Inode> {
        let (mount, _) = self.binding(context, false).await?;
        let _gate = self.runtime.lock_branch(mount.branch).await;
        let (mount, _) = self.binding(context, false).await?;
        let parent = self.node(&mount, parent).await?;
        Self::directory(&parent)?;
        Self::permission(context, &parent.inode, 1)?;
        let path = match name {
            "." => parent.path.clone(),
            ".." => parent.path.parent().unwrap_or_else(WorkspacePath::root),
            name => parent.path.join(name)?,
        };
        let node = self
            .nodes(&mount)
            .await?
            .into_iter()
            .find(|node| node.path.lookup_key() == path.lookup_key())
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "directory entry does not exist"))?;
        Ok(Self::local_attributes(&mount, node.inode))
    }

    async fn getattr(&self, context: &FsContext, inode: InodeId) -> Result<Inode> {
        let (mount, _) = self.binding(context, false).await?;
        let _gate = self.runtime.lock_branch(mount.branch).await;
        let (mount, _) = self.binding(context, false).await?;
        Ok(Self::local_attributes(
            &mount,
            self.node(&mount, inode).await?.inode,
        ))
    }

    async fn readdir(&self, context: &FsContext, inode: InodeId) -> Result<Vec<DirectoryItem>> {
        let (mount, _) = self.binding(context, false).await?;
        let _gate = self.runtime.lock_branch(mount.branch).await;
        let (mount, _) = self.binding(context, false).await?;
        let node = self.node(&mount, inode).await?;
        Self::directory(&node)?;
        Self::permission(context, &node.inode, 5)?;
        Ok(self
            .nodes(&mount)
            .await?
            .into_iter()
            .filter(|child| child.path.parent() == Some(node.path.clone()))
            .map(|child| DirectoryItem {
                name: child.path.name().to_owned(),
                inode: Self::local_attributes(&mount, child.inode),
            })
            .collect())
    }

    async fn create(
        &self,
        context: &FsContext,
        parent: InodeId,
        name: &str,
        kind: FileKind,
        mode: u32,
    ) -> Result<Inode> {
        let (mount, _) = self.binding(context, true).await?;
        let _gate = self.runtime.lock_branch(mount.branch).await;
        let (mount, branch) = self.binding(context, true).await?;
        let parent = self.node(&mount, parent).await?;
        Self::directory(&parent)?;
        Self::permission(context, &parent.inode, 3)?;
        let path = parent.path.join(name)?;
        let mut nodes = self.runtime.state.nodes(branch.id).await?;
        if nodes
            .iter()
            .any(|node| node.path.lookup_key() == path.lookup_key())
        {
            return Err(Error::new(
                ErrorCode::AlreadyExists,
                "directory entry already exists",
            ));
        }
        self.check_capacity(&branch, &nodes, 0, 0, 1).await?;
        let now = self.runtime.clock.now_ns();
        let inode = Inode {
            id: InodeId(branch.next_inode),
            kind,
            mode: mode & 0o777,
            uid: mount.identity.uid,
            gid: mount.identity.gid,
            size: 0,
            created_ns: now,
            modified_ns: now,
            content: None,
        };
        let node = Node {
            path,
            inode: inode.clone(),
            dirty: true,
        };
        if kind == FileKind::File {
            self.working
                .create(Self::working_file(&branch, &node), None)
                .await?;
        }
        for node in &mut nodes {
            if node.inode.id == parent.inode.id {
                node.inode.modified_ns = now;
            }
        }
        nodes.push(node);
        let next = increment(branch.next_inode)?;
        self.commit_nodes(
            branch.clone(),
            vec![NodeChange::Replace {
                branch: branch.id,
                nodes,
            }],
            Some(next),
        )
        .await?;
        Ok(inode)
    }

    async fn setattr(
        &self,
        context: &FsContext,
        inode: InodeId,
        attributes: AttributeUpdate,
    ) -> Result<Inode> {
        let (mount, _) = self.binding(context, true).await?;
        let _gate = self.runtime.lock_branch(mount.branch).await;
        let (mount, branch) = self.binding(context, true).await?;
        let mut node = self.node(&mount, inode).await?;
        if attributes.mode.is_none()
            && attributes.size.is_none()
            && attributes.modified_ns.is_none()
        {
            return Ok(Self::local_attributes(&mount, node.inode));
        }
        if attributes.size.is_some() && node.inode.kind != FileKind::File {
            return Err(Error::new(
                ErrorCode::IsDirectory,
                "cannot truncate a directory",
            ));
        }
        if let Some(size) = attributes.size {
            Self::permission(context, &node.inode, 2)?;
            self.check_capacity(
                &branch,
                &self.runtime.state.nodes(branch.id).await?,
                node.inode.size,
                size,
                0,
            )
            .await?;
        }
        if node.inode.kind == FileKind::File {
            self.prepare_write(&branch, &node).await?;
        }
        if let Some(size) = attributes.size {
            if let Err(error) = self
                .working
                .truncate(Self::working_file(&branch, &node), size)
                .await
            {
                return Err(self.stop_after_write_error(&branch, error).await);
            }
            node.inode.size = size;
        }
        if let Some(mode) = attributes.mode {
            node.inode.mode = mode & 0o777;
        }
        node.inode.modified_ns = attributes
            .modified_ns
            .unwrap_or_else(|| self.runtime.clock.now_ns());
        node.dirty = true;
        let inode = Self::local_attributes(&mount, node.inode.clone());
        if let Err(error) = self
            .commit_nodes(
                branch.clone(),
                vec![NodeChange::Put {
                    branch: branch.id,
                    node,
                }],
                None,
            )
            .await
        {
            return Err(self.stop_after_write_error(&branch, error).await);
        }
        Ok(inode)
    }

    async fn open(
        &self,
        context: &FsContext,
        inode: InodeId,
        mode: OpenMode,
    ) -> Result<FileHandle> {
        if (!mode.read && !mode.write) || (mode.append && !mode.write) {
            return Err(Error::invalid("invalid file open mode"));
        }
        let (mount, _) = self.binding(context, mode.write).await?;
        let _gate = self.runtime.lock_branch(mount.branch).await;
        let (mount, _) = self.binding(context, mode.write).await?;
        let node = self.node(&mount, inode).await?;
        if node.inode.kind != FileKind::File {
            return Err(Error::new(
                ErrorCode::IsDirectory,
                "cannot open a directory as a file",
            ));
        }
        Self::permission(
            context,
            &node.inode,
            (u32::from(mode.read) * 4) | (u32::from(mode.write) * 2),
        )?;
        let id = HandleId::new();
        self.handles.lock().insert(
            id,
            OpenFile {
                mount: mount.id,
                generation: mount.generation,
                branch: mount.branch,
                inode,
                mode,
            },
        );
        Ok(FileHandle { id, inode })
    }

    async fn read(
        &self,
        context: &FsContext,
        id: HandleId,
        offset: u64,
        size: u32,
    ) -> Result<Bytes> {
        if size as usize > IO_BUFFER_BYTES * 16 {
            return Err(Error::invalid("read request exceeds the buffer limit"));
        }
        let handle = self.handle(context, id)?;
        let _gate = self.runtime.lock_branch(handle.branch).await;
        let handle = self.handle(context, id)?;
        if !handle.mode.read {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "handle is not readable",
            ));
        }
        let (mount, branch) = self.binding(context, false).await?;
        let node = self.node(&mount, handle.inode).await?;
        if node.dirty {
            return self
                .working
                .read(Self::working_file(&branch, &node), offset, size)
                .await;
        }
        read_content(
            self.trees.content.as_ref(),
            branch.workspace,
            node.inode
                .content
                .as_ref()
                .ok_or_else(|| Error::integrity("file content is missing"))?,
            offset,
            size,
        )
        .await
    }

    async fn write(
        &self,
        context: &FsContext,
        id: HandleId,
        mut offset: u64,
        data: Bytes,
    ) -> Result<u32> {
        if data.len() > IO_BUFFER_BYTES * 16 {
            return Err(Error::invalid("write request exceeds the buffer limit"));
        }
        let handle = self.handle(context, id)?;
        let _gate = self.runtime.lock_branch(handle.branch).await;
        let handle = self.handle(context, id)?;
        if !handle.mode.write {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "handle is not writable",
            ));
        }
        let (mount, branch) = self.binding(context, true).await?;
        let mut node = self.node(&mount, handle.inode).await?;
        if data.is_empty() {
            return Ok(0);
        }
        if handle.mode.append {
            offset = node.inode.size;
        }
        let size = offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| Error::invalid("write range overflow"))?
            .max(node.inode.size);
        self.check_capacity(
            &branch,
            &self.runtime.state.nodes(branch.id).await?,
            node.inode.size,
            size,
            0,
        )
        .await?;
        self.prepare_write(&branch, &node).await?;
        let written = match self
            .working
            .write(Self::working_file(&branch, &node), offset, data)
            .await
        {
            Ok(written) => written,
            Err(error) => return Err(self.stop_after_write_error(&branch, error).await),
        };
        node.inode.size = size;
        node.inode.modified_ns = self.runtime.clock.now_ns();
        node.dirty = true;
        if let Err(error) = self
            .commit_nodes(
                branch.clone(),
                vec![NodeChange::Put {
                    branch: branch.id,
                    node,
                }],
                None,
            )
            .await
        {
            return Err(self.stop_after_write_error(&branch, error).await);
        }
        Ok(written)
    }

    async fn release(&self, context: &FsContext, id: HandleId) -> Result<()> {
        let handle = self.handle(context, id)?;
        let _gate = self.runtime.lock_branch(handle.branch).await;
        self.handle(context, id)?;
        self.handles.lock().remove(&id);
        Ok(())
    }

    async fn rename(
        &self,
        context: &FsContext,
        parent: InodeId,
        name: &str,
        new_parent: InodeId,
        new_name: &str,
        replace: bool,
    ) -> Result<()> {
        let (mount, _) = self.binding(context, true).await?;
        let _gate = self.runtime.lock_branch(mount.branch).await;
        let (mount, branch) = self.binding(context, true).await?;
        let parent = self.node(&mount, parent).await?;
        let new_parent = self.node(&mount, new_parent).await?;
        for directory in [&parent, &new_parent] {
            Self::directory(directory)?;
            Self::permission(context, &directory.inode, 3)?;
        }
        let source_path = parent.path.join(name)?;
        let destination = new_parent.path.join(new_name)?;
        let mut nodes = self.runtime.state.nodes(branch.id).await?;
        let source = nodes
            .iter()
            .find(|node| node.path.lookup_key() == source_path.lookup_key())
            .cloned()
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "rename source does not exist"))?;
        if source.path == destination {
            return Ok(());
        }
        if source.inode.kind == FileKind::Directory && source.path.contains(&new_parent.path) {
            return Err(Error::invalid("cannot move a directory into itself"));
        }
        let target = nodes
            .iter()
            .find(|node| {
                node.path.lookup_key() == destination.lookup_key()
                    && node.inode.id != source.inode.id
            })
            .cloned();
        if let Some(target) = &target {
            if !replace {
                return Err(Error::new(
                    ErrorCode::AlreadyExists,
                    "rename destination exists",
                ));
            }
            if target.inode.kind != source.inode.kind {
                return Err(Error::new(
                    if target.inode.kind == FileKind::Directory {
                        ErrorCode::IsDirectory
                    } else {
                        ErrorCode::NotDirectory
                    },
                    "rename destination has a different file kind",
                ));
            }
            if self.inode_open(branch.id, target.inode.id) {
                return Err(Error::new(
                    ErrorCode::Busy,
                    "rename destination has open handles",
                ));
            }
            if target.inode.kind == FileKind::Directory
                && nodes
                    .iter()
                    .any(|node| node.path.parent() == Some(target.path.clone()))
            {
                return Err(Error::new(
                    ErrorCode::DirectoryNotEmpty,
                    "rename destination directory is not empty",
                ));
            }
            nodes.retain(|node| node.inode.id != target.inode.id);
        }
        let now = self.runtime.clock.now_ns();
        for node in &mut nodes {
            if source.path.contains(&node.path) {
                let suffix = node
                    .path
                    .as_str()
                    .strip_prefix(source.path.as_str())
                    .ok_or_else(|| Error::integrity("directory path casing is inconsistent"))?;
                node.path = format!("{}{suffix}", destination.as_str()).try_into()?;
            }
            if node.inode.id == parent.inode.id || node.inode.id == new_parent.inode.id {
                node.inode.modified_ns = now;
            }
        }
        self.commit_nodes(
            branch.clone(),
            vec![NodeChange::Replace {
                branch: branch.id,
                nodes,
            }],
            None,
        )
        .await?;
        if let Some(target) = target
            && let Err(error) = self
                .working
                .remove(Self::working_file(&branch, &target))
                .await
        {
            tracing::warn!(%error, "obsolete working file will be removed during recovery");
        }
        Ok(())
    }

    async fn can_unlink(
        &self,
        context: &FsContext,
        parent: InodeId,
        name: &str,
        directory: bool,
        closing_handle: Option<HandleId>,
    ) -> Result<()> {
        let (mount, _) = self.binding(context, true).await?;
        let _gate = self.runtime.lock_branch(mount.branch).await;
        let (mount, branch) = self.binding(context, true).await?;
        let parent = self.node(&mount, parent).await?;
        Self::directory(&parent)?;
        Self::permission(context, &parent.inode, 3)?;
        let path = parent.path.join(name)?;
        let nodes = self.runtime.state.nodes(branch.id).await?;
        let target = nodes
            .iter()
            .find(|node| node.path.lookup_key() == path.lookup_key())
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "directory entry does not exist"))?;
        if directory != (target.inode.kind == FileKind::Directory) {
            return Err(Error::invalid("unlink file kind mismatch"));
        }
        if self.handles.lock().iter().any(|(id, handle)| {
            Some(*id) != closing_handle
                && handle.branch == branch.id
                && handle.inode == target.inode.id
        }) {
            return Err(Error::new(ErrorCode::Busy, "file has other open handles"));
        }
        if directory
            && nodes
                .iter()
                .any(|node| node.path.parent() == Some(target.path.clone()))
        {
            return Err(Error::new(
                ErrorCode::DirectoryNotEmpty,
                "directory is not empty",
            ));
        }
        Ok(())
    }

    async fn unlink(
        &self,
        context: &FsContext,
        parent: InodeId,
        name: &str,
        directory: bool,
    ) -> Result<()> {
        let (mount, _) = self.binding(context, true).await?;
        let _gate = self.runtime.lock_branch(mount.branch).await;
        let (mount, branch) = self.binding(context, true).await?;
        let parent = self.node(&mount, parent).await?;
        Self::directory(&parent)?;
        Self::permission(context, &parent.inode, 3)?;
        let path = parent.path.join(name)?;
        let mut nodes = self.runtime.state.nodes(branch.id).await?;
        let target = nodes
            .iter()
            .find(|node| node.path.lookup_key() == path.lookup_key())
            .cloned()
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "directory entry does not exist"))?;
        if directory != (target.inode.kind == FileKind::Directory) {
            return Err(Error::new(
                if directory {
                    ErrorCode::NotDirectory
                } else {
                    ErrorCode::IsDirectory
                },
                "unlink file kind mismatch",
            ));
        }
        if self.inode_open(branch.id, target.inode.id) {
            return Err(Error::new(ErrorCode::Busy, "file has open handles"));
        }
        if directory
            && nodes
                .iter()
                .any(|node| node.path.parent() == Some(target.path.clone()))
        {
            return Err(Error::new(
                ErrorCode::DirectoryNotEmpty,
                "directory is not empty",
            ));
        }
        nodes.retain(|node| node.inode.id != target.inode.id);
        for node in &mut nodes {
            if node.inode.id == parent.inode.id {
                node.inode.modified_ns = self.runtime.clock.now_ns();
            }
        }
        self.commit_nodes(
            branch.clone(),
            vec![NodeChange::Replace {
                branch: branch.id,
                nodes,
            }],
            None,
        )
        .await?;
        if let Err(error) = self
            .working
            .remove(Self::working_file(&branch, &target))
            .await
        {
            tracing::warn!(%error, "obsolete working file will be removed during recovery");
        }
        Ok(())
    }

    async fn fsync(&self, context: &FsContext) -> Result<DurabilityReceipt> {
        let (mount, branch) = self.binding(context, false).await?;
        if mount.access == AccessMode::ReadOnly {
            return Err(Error::new(
                ErrorCode::ReadOnly,
                "read-only mount has no writable durability boundary",
            ));
        }
        self.durability
            .flush(branch.id, mount.principal, mount.durability)
            .await
    }

    async fn statfs(&self, context: &FsContext) -> Result<StorageStats> {
        self.binding(context, false).await?;
        self.trees.content.local.stats().await
    }
}

#[async_trait]
impl WorkingTree for FileService {
    async fn replace(&self, branch: BranchId, root: &ObjectRef) -> Result<Vec<Node>> {
        let branch = self.runtime.branch(branch).await?;
        if self
            .handles
            .lock()
            .values()
            .any(|handle| handle.branch == branch.id)
        {
            return Err(Error::new(
                ErrorCode::MountBusy,
                "branch has open file handles",
            ));
        }
        Ok(self
            .trees
            .tree(branch.workspace, root)
            .await?
            .into_iter()
            .map(|(path, inode)| Node {
                path,
                inode,
                dirty: false,
            })
            .collect())
    }

    fn open_handles(&self, mount: MountId) -> usize {
        self.handles
            .lock()
            .values()
            .filter(|handle| handle.mount == mount)
            .count()
    }
    fn branch_dirty_time(&self, branch: BranchId) -> Option<(u64, u64)> {
        self.runtime
            .dirty
            .lock()
            .get(&branch)
            .map(|value| (value.first, value.last))
    }
    fn clear_dirty_time(&self, branch: BranchId, through_seq: u64) {
        self.runtime.clear_dirty(branch, through_seq);
    }
}

pub(crate) async fn read_content(
    content: &dyn ContentResolver,
    workspace: WorkspaceId,
    object: &ObjectRef,
    offset: u64,
    size: u32,
) -> Result<Bytes> {
    if size as usize > IO_BUFFER_BYTES * 16 {
        return Err(Error::invalid("read request exceeds the buffer limit"));
    }
    if offset >= object.size || size == 0 {
        return Ok(Bytes::new());
    }
    content.read_range(workspace, object, offset, size).await
}
