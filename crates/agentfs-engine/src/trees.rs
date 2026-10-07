use crate::{ContentService, Runtime};
use agentfs_model::*;
use agentfs_ports::*;
use async_trait::async_trait;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
};

#[derive(Debug)]
pub struct TreeService {
    runtime: Arc<Runtime>,
    pub(crate) content: Arc<ContentService>,
}

impl TreeService {
    pub fn new(runtime: Arc<Runtime>, content: Arc<ContentService>) -> Arc<Self> {
        Arc::new(Self { runtime, content })
    }

    pub(crate) async fn save_index<T: Serialize + Clone + Send + Sync>(
        &self,
        workspace: WorkspaceId,
        kind: ObjectKind,
        mut entries: Vec<IndexEntry<T>>,
    ) -> Result<ObjectRef> {
        entries.sort_by(|left, right| left.key.cmp(&right.key));
        if entries.windows(2).any(|pair| pair[0].key == pair[1].key) {
            return Err(Error::integrity("duplicate index key"));
        }
        if entries.is_empty() {
            return self
                .content
                .local
                .put(
                    workspace,
                    kind,
                    encode(&IndexPage::<T>::Leaf { entries })?.into(),
                )
                .await;
        }
        let mut pages = Vec::new();
        for chunk in entries.chunks(INDEX_PAGE_ENTRIES) {
            let object = self
                .content
                .local
                .put(
                    workspace,
                    kind,
                    encode(&IndexPage::Leaf {
                        entries: chunk.to_vec(),
                    })?
                    .into(),
                )
                .await?;
            pages.push(IndexChild {
                first_key: chunk[0].key.clone(),
                object,
            });
        }
        while pages.len() > 1 {
            let mut parents = Vec::new();
            for chunk in pages.chunks(INDEX_PAGE_ENTRIES) {
                let object = self
                    .content
                    .local
                    .put(
                        workspace,
                        kind,
                        encode(&IndexPage::<T>::Branch {
                            children: chunk.to_vec(),
                        })?
                        .into(),
                    )
                    .await?;
                parents.push(IndexChild {
                    first_key: chunk[0].first_key.clone(),
                    object,
                });
            }
            pages = parents;
        }
        Ok(pages.remove(0).object)
    }

    pub(crate) async fn load_index<T: DeserializeOwned + Send>(
        &self,
        workspace: WorkspaceId,
        root: &ObjectRef,
        limit: usize,
    ) -> Result<Vec<IndexEntry<T>>> {
        let mut pending = vec![(root.clone(), None::<String>, None::<String>, 0u32)];
        let mut visited = BTreeSet::new();
        let mut result = Vec::new();
        while let Some((object, lower, upper, depth)) = pending.pop() {
            if object.kind != root.kind
                || depth > 32
                || !visited.insert(object.id.clone())
                || visited.len() > limit.saturating_mul(2).max(64)
            {
                return Err(Error::integrity("invalid index graph"));
            }
            let page: IndexPage<T> = decode(&self.content.metadata(workspace, &object).await?)?;
            match page {
                IndexPage::Leaf { entries } => {
                    if entries.len() > INDEX_PAGE_ENTRIES || (depth != 0 && entries.is_empty()) {
                        return Err(Error::integrity("invalid index page size"));
                    }
                    if lower.as_ref().is_some_and(|bound| {
                        entries.first().is_none_or(|entry| &entry.key != bound)
                    }) || entries.windows(2).any(|pair| pair[0].key >= pair[1].key)
                        || upper.as_ref().is_some_and(|bound| {
                            entries.last().is_some_and(|entry| &entry.key >= bound)
                        })
                    {
                        return Err(Error::integrity(
                            "index entries are not ordered within their parent range",
                        ));
                    }
                    result.extend(entries);
                    if result.len() > limit {
                        return Err(Error::new(
                            ErrorCode::CapacityExceeded,
                            "index exceeds the configured entry limit",
                        ));
                    }
                }
                IndexPage::Branch { children } => {
                    if children.is_empty()
                        || children.len() > INDEX_PAGE_ENTRIES
                        || lower
                            .as_ref()
                            .is_some_and(|bound| &children[0].first_key != bound)
                        || children
                            .windows(2)
                            .any(|pair| pair[0].first_key >= pair[1].first_key)
                        || upper
                            .as_ref()
                            .is_some_and(|bound| &children[children.len() - 1].first_key >= bound)
                    {
                        return Err(Error::integrity("invalid index child range"));
                    }
                    for (index, child) in children.iter().enumerate().rev() {
                        let end = children
                            .get(index + 1)
                            .map(|next| next.first_key.clone())
                            .or_else(|| upper.clone());
                        pending.push((
                            child.object.clone(),
                            Some(child.first_key.clone()),
                            end,
                            depth + 1,
                        ));
                    }
                }
            }
        }
        Ok(result)
    }

    pub(crate) async fn children(
        &self,
        workspace: WorkspaceId,
        reference: &ObjectRef,
    ) -> Result<Vec<ObjectRef>> {
        if reference.kind == ObjectKind::File {
            return Ok(vec![]);
        }
        let data = self.content.metadata(workspace, reference).await?;
        let mut children = Vec::new();
        match reference.kind {
            ObjectKind::Tree => {
                let root: TreeRoot = decode(&data)?;
                if root.format_version != FORMAT_VERSION
                    || root.directories.kind != ObjectKind::DirectoryIndex
                    || root.inodes.kind != ObjectKind::InodeIndex
                {
                    return Err(Error::integrity("invalid tree root"));
                }
                children.extend([root.directories, root.inodes]);
            }
            ObjectKind::InodeIndex => match decode::<IndexPage<Inode>>(&data)? {
                IndexPage::Branch { children: pages } => {
                    children.extend(pages.into_iter().map(|page| page.object))
                }
                IndexPage::Leaf { entries } => {
                    children.extend(entries.into_iter().filter_map(|entry| entry.value.content))
                }
            },
            ObjectKind::DirectoryIndex => {
                if let IndexPage::Branch { children: pages } =
                    decode::<IndexPage<DirectoryEntry>>(&data)?
                {
                    children.extend(pages.into_iter().map(|page| page.object));
                }
            }
            ObjectKind::RecordIndex => match decode::<IndexPage<HistoryRecord>>(&data)? {
                IndexPage::Branch { children: pages } => {
                    children.extend(pages.into_iter().map(|page| page.object))
                }
                IndexPage::Leaf { entries } => {
                    for entry in entries {
                        if let HistoryRecord::Revision(revision) = entry.value {
                            children.push(revision.root);
                        }
                    }
                }
            },
            ObjectKind::File => (),
        }
        Ok(children)
    }

    pub(crate) async fn closure(
        &self,
        workspace: WorkspaceId,
        roots: Vec<ObjectRef>,
    ) -> Result<Vec<ObjectRef>> {
        let mut result = BTreeMap::new();
        let mut pending = roots;
        while let Some(reference) = pending.pop() {
            if let Some(existing) = result.get(&reference.id) {
                if existing != &reference {
                    return Err(Error::integrity("object has conflicting references"));
                }
                continue;
            }
            pending.extend(self.children(workspace, &reference).await?);
            result.insert(reference.id.clone(), reference);
        }
        Ok(result.into_values().collect())
    }
}

fn inode_key(id: InodeId) -> String {
    format!("{:020}", id.0)
}
fn directory_key(entry: &DirectoryEntry) -> Result<String> {
    Ok(format!(
        "{}/{}",
        inode_key(entry.parent),
        WorkspacePath::root().join(&entry.name)?.lookup_key()
    ))
}

pub(crate) fn validate_tree(tree: &FileTree, workspace: &Workspace) -> Result<()> {
    if tree.len() as u64 > workspace.max_inodes {
        return Err(Error::new(
            ErrorCode::CapacityExceeded,
            "workspace inode limit exceeded",
        ));
    }
    let root = tree
        .get(&WorkspacePath::root())
        .ok_or_else(|| Error::integrity("tree root is missing"))?;
    if root.kind != FileKind::Directory || root.id != InodeId::ROOT {
        return Err(Error::integrity("invalid root inode"));
    }
    let mut names = BTreeSet::new();
    let mut ids = BTreeSet::new();
    let mut total = 0u64;
    for (path, inode) in tree {
        if !names.insert(path.lookup_key())
            || !ids.insert(inode.id)
            || inode.id.0 == 0
            || inode.id.0 > i64::MAX as u64
        {
            return Err(Error::integrity("duplicate name or invalid inode identity"));
        }
        if inode.mode & !0o777 != 0 {
            return Err(Error::integrity("unsupported inode permission bits"));
        }
        if let Some(parent) = path.parent()
            && tree
                .get(&parent)
                .is_none_or(|inode| inode.kind != FileKind::Directory)
        {
            return Err(Error::integrity("parent directory is missing"));
        }
        match inode.kind {
            FileKind::File => {
                if inode.content.as_ref().is_none_or(|content| {
                    content.kind != ObjectKind::File || content.size != inode.size
                }) {
                    return Err(Error::integrity("file content does not match inode"));
                }
                if inode.size > workspace.max_file_bytes {
                    return Err(Error::new(
                        ErrorCode::CapacityExceeded,
                        "file exceeds workspace size limit",
                    ));
                }
                total = total
                    .checked_add(inode.size)
                    .ok_or_else(|| Error::integrity("tree size overflow"))?;
            }
            FileKind::Directory => {
                if inode.content.is_some() || inode.size != 0 {
                    return Err(Error::integrity("directory has file content"));
                }
            }
        }
    }
    if total > workspace.max_working_bytes {
        return Err(Error::new(
            ErrorCode::CapacityExceeded,
            "workspace size limit exceeded",
        ));
    }
    Ok(())
}

#[async_trait]
impl RevisionReader for TreeService {
    async fn revision(&self, workspace: WorkspaceId, id: RevisionId) -> Result<Revision> {
        let revision = self
            .runtime
            .state
            .revision(id)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "revision does not exist"))?;
        if revision.workspace != workspace {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "revision belongs to another workspace",
            ));
        }
        Ok(revision)
    }

    async fn tree(&self, workspace: WorkspaceId, root: &ObjectRef) -> Result<FileTree> {
        if root.kind != ObjectKind::Tree {
            return Err(Error::integrity("expected a tree object"));
        }
        let policy = self
            .runtime
            .state
            .workspace(workspace)
            .await?
            .ok_or_else(|| Error::new(ErrorCode::NotFound, "workspace does not exist"))?;
        let root: TreeRoot = decode(&self.content.metadata(workspace, root).await?)?;
        if root.format_version != FORMAT_VERSION
            || root.directories.kind != ObjectKind::DirectoryIndex
            || root.inodes.kind != ObjectKind::InodeIndex
        {
            return Err(Error::integrity("unsupported tree format"));
        }
        let limit = usize::try_from(policy.max_inodes)
            .map_err(|_| Error::integrity("inode limit exceeds platform capacity"))?;
        let mut inodes = BTreeMap::new();
        for entry in self
            .load_index::<Inode>(workspace, &root.inodes, limit)
            .await?
        {
            if entry.key != inode_key(entry.value.id) {
                return Err(Error::integrity("inode key mismatch"));
            }
            inodes.insert(entry.value.id, entry.value);
        }
        let mut directories = BTreeMap::<InodeId, Vec<DirectoryEntry>>::new();
        for entry in self
            .load_index::<DirectoryEntry>(workspace, &root.directories, limit)
            .await?
        {
            if entry.key != directory_key(&entry.value)? {
                return Err(Error::integrity("directory key mismatch"));
            }
            directories
                .entry(entry.value.parent)
                .or_default()
                .push(entry.value);
        }
        let mut tree = BTreeMap::new();
        let mut queue = VecDeque::from([(WorkspacePath::root(), InodeId::ROOT)]);
        while let Some((path, id)) = queue.pop_front() {
            let inode = inodes.remove(&id).ok_or_else(|| {
                Error::integrity("directory refers to a missing or repeated inode")
            })?;
            if let Some(entries) = directories.remove(&id) {
                if inode.kind != FileKind::Directory {
                    return Err(Error::integrity("file has directory entries"));
                }
                for entry in entries {
                    queue.push_back((path.join(&entry.name)?, entry.inode));
                }
            }
            if tree.insert(path, inode).is_some() {
                return Err(Error::integrity("repeated directory entry"));
            }
        }
        if !inodes.is_empty() || !directories.is_empty() {
            return Err(Error::integrity("tree contains unreachable inodes"));
        }
        validate_tree(&tree, &policy)?;
        Ok(tree)
    }

    async fn save_tree(&self, workspace: WorkspaceId, tree: &FileTree) -> Result<ObjectRef> {
        if let Some(policy) = self.runtime.state.workspace(workspace).await? {
            validate_tree(tree, &policy)?;
        }
        let mut directories = Vec::new();
        let mut inodes = Vec::new();
        for (path, inode) in tree {
            inodes.push(IndexEntry {
                key: inode_key(inode.id),
                value: inode.clone(),
            });
            if let Some(parent) = path.parent() {
                let parent = tree
                    .get(&parent)
                    .ok_or_else(|| Error::integrity("parent directory is missing"))?;
                let entry = DirectoryEntry {
                    parent: parent.id,
                    name: path.name().to_owned(),
                    inode: inode.id,
                };
                directories.push(IndexEntry {
                    key: directory_key(&entry)?,
                    value: entry,
                });
            }
        }
        let directories = self
            .save_index(workspace, ObjectKind::DirectoryIndex, directories)
            .await?;
        let inodes = self
            .save_index(workspace, ObjectKind::InodeIndex, inodes)
            .await?;
        self.content
            .local
            .put(
                workspace,
                ObjectKind::Tree,
                encode(&TreeRoot {
                    format_version: FORMAT_VERSION,
                    directories,
                    inodes,
                })?
                .into(),
            )
            .await
    }
}
