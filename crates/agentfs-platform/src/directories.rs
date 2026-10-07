use agentfs_model::*;
use agentfs_ports::*;
use async_trait::async_trait;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug)]
pub struct HostDirectories {
    roots: Vec<(PathBuf, Arc<Dir>)>,
}

impl HostDirectories {
    pub fn new(roots: Vec<PathBuf>) -> Result<Arc<Self>> {
        let mut allowed = Vec::new();
        for root in roots {
            let root = root.canonicalize()?;
            let directory = Dir::open_ambient_dir(&root, ambient_authority())?;
            allowed.push((root, Arc::new(directory)));
        }
        allowed.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
        Ok(Arc::new(Self { roots: allowed }))
    }

    fn relative(&self, path: &Path) -> Result<(Arc<Dir>, PathBuf)> {
        if !path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
        {
            return Err(Error::invalid("host path must be absolute and normalized"));
        }
        let parent = path
            .parent()
            .ok_or_else(|| Error::invalid("host path must name a directory"))?
            .canonicalize()?;
        let path = parent.join(
            path.file_name()
                .ok_or_else(|| Error::invalid("host path must name a directory"))?,
        );
        for (root, directory) in &self.roots {
            if let Ok(relative) = path.strip_prefix(root) {
                return Ok((directory.clone(), relative.to_owned()));
            }
        }
        Err(Error::new(
            ErrorCode::PermissionDenied,
            "host path is outside the configured directory roots",
        ))
    }

    fn open_directory(root: &Dir, relative: &Path) -> Result<Dir> {
        let mut directory = root.try_clone()?;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(Error::invalid("invalid relative host path"));
            };
            directory = directory.open_dir_nofollow(name)?;
        }
        Ok(directory)
    }

    fn file_options() -> OpenOptions {
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No).nonblock(true);
        options
    }
}

fn timestamp(value: std::io::Result<cap_std::time::SystemTime>) -> i64 {
    value
        .ok()
        .map(cap_std::time::SystemTime::into_std)
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|time| time.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}
fn attributes(metadata: &cap_std::fs::Metadata, id: InodeId) -> Result<Inode> {
    let kind = if metadata.is_dir() {
        FileKind::Directory
    } else if metadata.is_file() {
        FileKind::File
    } else {
        return Err(Error::new(
            ErrorCode::Unsupported,
            "import supports regular files and directories",
        ));
    };
    #[cfg(unix)]
    let (mode, uid, gid) = {
        use cap_std::fs::MetadataExt;
        (metadata.mode() & 0o777, metadata.uid(), metadata.gid())
    };
    #[cfg(windows)]
    let (mode, uid, gid) = (
        if kind == FileKind::Directory {
            0o755
        } else if metadata.permissions().readonly() {
            0o444
        } else {
            0o644
        },
        1,
        1,
    );
    Ok(Inode {
        id,
        kind,
        mode,
        uid,
        gid,
        size: if kind == FileKind::File {
            metadata.len()
        } else {
            0
        },
        created_ns: timestamp(metadata.created()),
        modified_ns: timestamp(metadata.modified()),
        content: None,
    })
}

#[async_trait]
impl DirectoryAccess for HostDirectories {
    async fn import_tree(
        &self,
        workspace: &Workspace,
        source: &Path,
        objects: Arc<dyn LocalObjectStore>,
    ) -> Result<FileTree> {
        let (root, relative) = self.relative(source)?;
        let workspace = workspace.clone();
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || runtime.block_on(async move {
            let source = Arc::new(Self::open_directory(&root, &relative)?);
            let root_metadata = source.dir_metadata()?;
            let mut tree = FileTree::from([(WorkspacePath::root(), attributes(&root_metadata, InodeId::ROOT)?)]);
            let mut pending = vec![(WorkspacePath::root(), PathBuf::new())];
            let mut seen = BTreeSet::from([WorkspacePath::root().lookup_key()]);
            let mut signatures = Vec::new(); let mut next_inode = 2u64; let mut total = 0u64;
            while let Some((parent, relative)) = pending.pop() {
                let directory = Self::open_directory(&source, &relative)?;
                let before = directory.dir_metadata()?;
                let mut entries = directory.entries()?.collect::<std::io::Result<Vec<_>>>()?;
                entries.sort_by_key(|entry| entry.file_name());
                for entry in entries {
                    if tree.len() as u64 >= workspace.max_inodes { return Err(Error::new(ErrorCode::CapacityExceeded, "import exceeds the inode limit")); }
                    let name = entry.file_name().into_string().map_err(|_| Error::invalid("import names must be UTF-8"))?;
                    let path = parent.join(&name)?;
                    if !seen.insert(path.lookup_key()) { return Err(Error::new(ErrorCode::NameConflict, "import contains names that collide under the workspace name policy")); }
                    let metadata = directory.symlink_metadata(&name)?;
                    if metadata.file_type().is_symlink() { return Err(Error::new(ErrorCode::Unsupported, "symbolic links cannot be imported")); }
                    let mut inode = attributes(&metadata, InodeId(next_inode))?; next_inode += 1;
                    if inode.kind == FileKind::Directory {
                        pending.push((path.clone(), PathBuf::from(path.as_str().trim_start_matches('/'))));
                    } else {
                        if inode.size > workspace.max_file_bytes { return Err(Error::new(ErrorCode::CapacityExceeded, "imported file exceeds the size limit")); }
                        total = total.checked_add(inode.size).ok_or_else(|| Error::invalid("import size overflow"))?;
                        if total > workspace.max_working_bytes { return Err(Error::new(ErrorCode::CapacityExceeded, "import exceeds workspace size limit")); }
                        let file = directory.open_with(&name, &Self::file_options())?;
                        let opened = file.metadata()?;
                        if !opened.is_file() || opened.len() != metadata.len() || opened.modified()? != metadata.modified()? { return Err(Error::new(ErrorCode::Busy, "import source changed during traversal")); }
                        let object = objects.ingest(workspace.id, ObjectKind::File, Box::pin(tokio::fs::File::from_std(file.into_std()))).await?;
                        let after = directory.symlink_metadata(&name)?;
                        if object.size != inode.size || after.len() != metadata.len() || after.modified()? != metadata.modified()? { return Err(Error::new(ErrorCode::Busy, "import source changed during capture")); }
                        inode.content = Some(object);
                    }
                    signatures.push((path.clone(), inode.kind, metadata.len(), metadata.modified()?));
                    tree.insert(path, inode);
                }
                if directory.dir_metadata()?.modified()? != before.modified()? { return Err(Error::new(ErrorCode::Busy, "import directory changed during capture")); }
            }
            for (path, kind, size, modified) in signatures {
                let relative = Path::new(path.as_str().trim_start_matches('/'));
                let metadata = source.symlink_metadata(relative)?;
                if metadata.file_type().is_symlink() || metadata.is_dir() != (kind == FileKind::Directory) || metadata.len() != size || metadata.modified()? != modified { return Err(Error::new(ErrorCode::Busy, "import source changed before capture completed")); }
            }
            if source.dir_metadata()?.modified()? != root_metadata.modified()? { return Err(Error::new(ErrorCode::Busy, "import root changed during capture")); }
            Ok(tree)
        })).await.map_err(|error| Error::new(ErrorCode::Io, format!("directory import task failed: {error}")))?
    }

    async fn export_tree(
        &self,
        workspace: WorkspaceId,
        tree: FileTree,
        destination: &Path,
        content: Arc<dyn ContentResolver>,
    ) -> Result<()> {
        let (root, relative) = self.relative(destination)?;
        let name = relative
            .file_name()
            .ok_or_else(|| Error::invalid("export destination must be a new child directory"))?
            .to_owned();
        let parent_path = relative.parent().unwrap_or_else(|| Path::new(""));
        let parent = Self::open_directory(&root, parent_path)?;
        if parent.symlink_metadata(&name).is_ok() {
            return Err(Error::new(
                ErrorCode::AlreadyExists,
                "export destination already exists",
            ));
        }
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            runtime.block_on(async move {
                let temporary = format!(".agentfs-export-{}", uuid::Uuid::new_v4());
                parent.create_dir(&temporary)?;
                let result = async {
                    let staging = parent.open_dir_nofollow(&temporary)?;
                    write_tree(&staging, workspace, &tree, content).await?;
                    // Windows directory handles prevent rename until released.
                    drop(staging);
                    #[cfg(unix)]
                    {
                        rustix::fs::renameat_with(
                            &parent,
                            &temporary,
                            &parent,
                            &name,
                            rustix::fs::RenameFlags::NOREPLACE,
                        )
                        .map_err(std::io::Error::from)?;
                        parent.try_clone()?.into_std_file().sync_all()?;
                    }
                    #[cfg(windows)]
                    {
                        if parent.symlink_metadata(&name).is_ok() {
                            return Err(Error::new(
                                ErrorCode::AlreadyExists,
                                "export destination already exists",
                            ));
                        }
                        parent.rename(&temporary, &parent, &name)?;
                    }
                    Ok(())
                }
                .await;
                if result.is_err() {
                    let _ = parent.remove_dir_all(&temporary);
                }
                result
            })
        })
        .await
        .map_err(|error| {
            Error::new(
                ErrorCode::Io,
                format!("directory export task failed: {error}"),
            )
        })?
    }
}

pub(crate) async fn write_tree(
    directory: &Dir,
    workspace: WorkspaceId,
    tree: &FileTree,
    content: Arc<dyn ContentResolver>,
) -> Result<()> {
    for (path, inode) in tree {
        if path.is_root() {
            continue;
        }
        let relative = Path::new(path.as_str().trim_start_matches('/'));
        match inode.kind {
            FileKind::Directory => directory.create_dir(relative)?,
            FileKind::File => {
                let mut options = OpenOptions::new();
                options
                    .write(true)
                    .create_new(true)
                    .follow(FollowSymlinks::No);
                let file = directory.open_with(relative, &options)?.into_std();
                let mut output = tokio::fs::File::from_std(file);
                let reference = inode
                    .content
                    .as_ref()
                    .ok_or_else(|| Error::integrity("exported file lacks content"))?;
                let stream = content.open(workspace, reference).await?;
                let copied =
                    tokio::io::copy(&mut stream.reader.take(reference.size + 1), &mut output)
                        .await?;
                if copied != reference.size {
                    return Err(Error::integrity("export content length mismatch"));
                }
                output.flush().await?;
                let output = output.into_std().await;
                output.set_times(
                    std::fs::FileTimes::new().set_modified(modified_time(inode.modified_ns)),
                )?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    output.set_permissions(std::fs::Permissions::from_mode(inode.mode))?;
                }
                #[cfg(windows)]
                {
                    let mut permissions = output.metadata()?.permissions();
                    permissions.set_readonly(inode.mode & 0o222 == 0);
                    output.set_permissions(permissions)?;
                }
                output.sync_all()?;
            }
        }
    }
    {
        for (path, inode) in tree
            .iter()
            .rev()
            .filter(|(_, inode)| inode.kind == FileKind::Directory)
        {
            let relative = Path::new(if path.is_root() {
                "."
            } else {
                path.as_str().trim_start_matches('/')
            });
            let child = directory.open_dir(relative)?;
            directory.set_times(
                relative,
                None,
                Some(cap_fs_ext::SystemTimeSpec::Absolute(
                    cap_std::time::SystemTime::from_std(modified_time(inode.modified_ns)),
                )),
            )?;
            #[cfg(unix)]
            {
                use cap_std::fs::PermissionsExt;
                directory
                    .set_permissions(relative, cap_std::fs::Permissions::from_mode(inode.mode))?;
                child.into_std_file().sync_all()?;
            }
            #[cfg(windows)]
            drop(child);
        }
    }
    Ok(())
}

fn modified_time(nanoseconds: i64) -> std::time::SystemTime {
    let elapsed = Duration::from_nanos(nanoseconds.unsigned_abs());
    if nanoseconds >= 0 {
        UNIX_EPOCH + elapsed
    } else {
        UNIX_EPOCH - elapsed
    }
}
