use agentfs_model::*;
use agentfs_ports::{AttributeUpdate, FileSystem, MountSpec, NativeMountState};
use bytes::Bytes;
use fuser::{
    BackgroundSession, BsdFileFlags, Config, Errno, FileAttr, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, KernelConfig, LockOwner, MountOption, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
    ReplyWrite, Request, TimeOrNow, WriteFlags,
};
use parking_lot::Mutex;
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    path::Path,
    process::Stdio,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{process::Command, runtime::Handle};

pub(super) struct Driver {
    mounts: Arc<Mutex<BTreeMap<MountId, (MountBinding, BackgroundSession)>>>,
}
impl Driver {
    pub fn new() -> Result<Self> {
        Ok(Self {
            mounts: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    pub async fn mount(&self, spec: MountSpec, files: Arc<dyn FileSystem>) -> Result<()> {
        let mounts = self.mounts.clone();
        let runtime = Handle::current();
        tokio::task::spawn_blocking(move || {
            let binding = spec.binding;
            std::fs::create_dir_all(&binding.path)?;
            if std::fs::read_dir(&binding.path)?.next().is_some() {
                return Err(Error::new(
                    ErrorCode::MountBusy,
                    "mount directory must be empty",
                ));
            }
            let mut config = Config::default();
            config.mount_options = vec![
                MountOption::FSName(format!("agentfs:{}", binding.id)),
                MountOption::Subtype("agentfs".into()),
                MountOption::DefaultPermissions,
                MountOption::NoDev,
                MountOption::NoSuid,
                MountOption::NoAtime,
                if binding.access == AccessMode::ReadOnly {
                    MountOption::RO
                } else {
                    MountOption::RW
                },
            ];
            config.n_threads = Some(
                std::thread::available_parallelism()
                    .map(usize::from)
                    .unwrap_or(2)
                    .clamp(2, 8),
            );
            let filesystem = Fuse {
                binding: binding.clone(),
                files,
                runtime,
                next_handle: Mutex::new(1),
                handles: Mutex::new(BTreeMap::new()),
                directories: Mutex::new(BTreeMap::new()),
            };
            let session = fuser::spawn_mount(filesystem, &binding.path, &config)?;
            mounts.lock().insert(binding.id, (binding, session));
            Ok(())
        })
        .await
        .map_err(|error| Error::new(ErrorCode::Io, error.to_string()))?
    }

    pub async fn status(&self, binding: &MountBinding) -> Result<NativeMountState> {
        if let Some((current, session)) = self.mounts.lock().get(&binding.id)
            && current.generation == binding.generation
            && current.path == binding.path
            && !session.guard.is_finished()
        {
            return Ok(NativeMountState::Mounted);
        }
        let path = binding.path.clone();
        tokio::task::spawn_blocking(move || {
            native_source(&path).map(|source| {
                if source.is_some() {
                    NativeMountState::Unknown
                } else {
                    NativeMountState::Absent
                }
            })
        })
        .await
        .map_err(|error| Error::new(ErrorCode::Io, error.to_string()))?
    }

    pub async fn unmount(&self, binding: &MountBinding) -> Result<()> {
        let path = binding.path.clone();
        let source = tokio::task::spawn_blocking(move || native_source(&path))
            .await
            .map_err(|error| Error::new(ErrorCode::Io, error.to_string()))??;
        if let Some(source) = source {
            if source != format!("agentfs:{}", binding.id) {
                return Err(Error::new(
                    ErrorCode::MountBusy,
                    "mount path belongs to a different filesystem",
                ));
            }
            #[cfg(target_os = "linux")]
            let mut command = {
                let program = if Path::new("/usr/bin/fusermount3").exists() {
                    "/usr/bin/fusermount3"
                } else {
                    "/bin/fusermount"
                };
                let mut command = Command::new(program);
                command.arg("-u");
                command
            };
            #[cfg(target_os = "macos")]
            let mut command = Command::new("/sbin/umount");
            let output = command
                .arg(&binding.path)
                .stdin(Stdio::null())
                .output()
                .await?;
            if !output.status.success() {
                return Err(Error::new(
                    ErrorCode::MountBusy,
                    format!(
                        "native unmount failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    ),
                ));
            }
        }
        let session = self
            .mounts
            .lock()
            .remove(&binding.id)
            .map(|(_, session)| session);
        if let Some(session) = session {
            tokio::task::spawn_blocking(move || session.join())
                .await
                .map_err(|error| Error::new(ErrorCode::Io, error.to_string()))??;
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn native_source(path: &Path) -> Result<Option<String>> {
    use std::os::unix::ffi::OsStrExt;
    let table = std::fs::read_to_string("/proc/self/mountinfo")?;
    for line in table.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let fields: Vec<_> = left.split_whitespace().collect();
        if fields.len() < 5 {
            continue;
        }
        let mut decoded = Vec::new();
        let bytes = fields[4].as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'\\'
                && index + 3 < bytes.len()
                && bytes[index + 1..index + 4]
                    .iter()
                    .all(|byte| matches!(byte, b'0'..=b'7'))
            {
                decoded.push(
                    (bytes[index + 1] - b'0') * 64
                        + (bytes[index + 2] - b'0') * 8
                        + bytes[index + 3]
                        - b'0',
                );
                index += 4;
            } else {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
        if decoded == path.as_os_str().as_bytes() {
            return Ok(right.split_whitespace().nth(1).map(str::to_owned));
        }
    }
    Ok(None)
}
#[cfg(target_os = "macos")]
fn native_source(path: &Path) -> Result<Option<String>> {
    let output = std::process::Command::new("/sbin/mount").output()?;
    let marker = format!(" on {} (", path.display());
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            line.split_once(&marker)
                .map(|(source, _)| source.to_owned())
        }))
}

type DirectorySnapshot = Vec<(String, Inode)>;
struct Fuse {
    binding: MountBinding,
    files: Arc<dyn FileSystem>,
    runtime: Handle,
    next_handle: Mutex<u64>,
    handles: Mutex<BTreeMap<u64, HandleId>>,
    directories: Mutex<BTreeMap<u64, DirectorySnapshot>>,
}
impl Fuse {
    fn context(&self, request: &Request) -> FsContext {
        FsContext {
            mount: self.binding.id,
            generation: self.binding.generation,
            identity: LocalIdentity {
                uid: request.uid(),
                gid: request.gid(),
            },
        }
    }
    fn next(&self) -> Result<u64> {
        let mut counter = self.next_handle.lock();
        let id = *counter;
        *counter = counter.checked_add(1).ok_or_else(|| {
            Error::new(ErrorCode::CapacityExceeded, "file handle space exhausted")
        })?;
        Ok(id)
    }
    fn handle(&self, handle: fuser::FileHandle) -> Result<HandleId> {
        self.handles
            .lock()
            .get(&handle.0)
            .copied()
            .ok_or_else(|| Error::new(ErrorCode::StaleBinding, "native file handle is closed"))
    }
    fn name<'a>(&self, name: &'a OsStr) -> Result<&'a str> {
        name.to_str()
            .ok_or_else(|| Error::invalid("file name must be UTF-8"))
    }
    fn open_file(&self, context: &FsContext, inode: InodeId, flags: i32) -> Result<u64> {
        let handle = self
            .runtime
            .block_on(self.files.open(context, inode, open_mode(flags)))?;
        let id = self.next()?;
        self.handles.lock().insert(id, handle.id);
        Ok(id)
    }
    fn empty(&self, result: Result<()>, reply: ReplyEmpty) {
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(error)),
        }
    }
}

fn open_mode(flags: i32) -> OpenMode {
    OpenMode {
        read: flags & libc::O_ACCMODE != libc::O_WRONLY,
        write: flags & libc::O_ACCMODE != libc::O_RDONLY,
        append: flags & libc::O_APPEND != 0,
    }
}
fn file_type(kind: FileKind) -> FileType {
    match kind {
        FileKind::File => FileType::RegularFile,
        FileKind::Directory => FileType::Directory,
    }
}
fn system_time(nanoseconds: i64) -> SystemTime {
    if nanoseconds >= 0 {
        UNIX_EPOCH + Duration::from_nanos(nanoseconds as u64)
    } else {
        UNIX_EPOCH - Duration::from_nanos(nanoseconds.unsigned_abs())
    }
}
fn time_ns(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(value) => value.as_nanos().min(i64::MAX as u128) as i64,
        Err(value) => -(value.duration().as_nanos().min(i64::MAX as u128) as i64),
    }
}
fn attr(inode: Inode) -> FileAttr {
    FileAttr {
        ino: INodeNo(inode.id.0),
        size: inode.size,
        blocks: inode.size.div_ceil(512),
        atime: system_time(inode.modified_ns),
        mtime: system_time(inode.modified_ns),
        ctime: system_time(inode.modified_ns),
        crtime: system_time(inode.created_ns),
        kind: file_type(inode.kind),
        perm: inode.mode as u16,
        nlink: if inode.kind == FileKind::Directory {
            2
        } else {
            1
        },
        uid: inode.uid,
        gid: inode.gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}
fn errno(error: Error) -> Errno {
    match error.code {
        ErrorCode::NotFound => Errno::ENOENT,
        ErrorCode::AlreadyExists | ErrorCode::NameConflict => Errno::EEXIST,
        ErrorCode::NotDirectory => Errno::ENOTDIR,
        ErrorCode::IsDirectory => Errno::EISDIR,
        ErrorCode::DirectoryNotEmpty => Errno::ENOTEMPTY,
        ErrorCode::PermissionDenied => Errno::EACCES,
        ErrorCode::ReadOnly => Errno::EROFS,
        ErrorCode::InvalidArgument => Errno::EINVAL,
        ErrorCode::CapacityExceeded => Errno::ENOSPC,
        ErrorCode::Busy | ErrorCode::MountBusy | ErrorCode::BranchBusy => Errno::EBUSY,
        ErrorCode::Unsupported => Errno::EOPNOTSUPP,
        ErrorCode::StaleBinding | ErrorCode::StaleAuthority => Errno::ESTALE,
        _ => Errno::EIO,
    }
}

impl Filesystem for Fuse {
    fn init(&mut self, _: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        let _ = config.set_max_write(IO_BUFFER_BYTES as u32);
        let _ = config.set_max_readahead(0);
        Ok(())
    }
    fn lookup(&self, request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let result = self.name(name).and_then(|name| {
            self.runtime.block_on(self.files.lookup(
                &self.context(request),
                InodeId(parent.0),
                name,
            ))
        });
        match result {
            Ok(inode) => reply.entry(
                &Duration::ZERO,
                &attr(inode),
                Generation(self.binding.generation),
            ),
            Err(error) => reply.error(errno(error)),
        }
    }
    fn getattr(
        &self,
        request: &Request,
        inode: INodeNo,
        _: Option<fuser::FileHandle>,
        reply: ReplyAttr,
    ) {
        match self
            .runtime
            .block_on(self.files.getattr(&self.context(request), InodeId(inode.0)))
        {
            Ok(inode) => reply.attr(&Duration::ZERO, &attr(inode)),
            Err(error) => reply.error(errno(error)),
        }
    }
    fn setattr(
        &self,
        request: &Request,
        inode: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _: Option<SystemTime>,
        _: Option<fuser::FileHandle>,
        crtime: Option<SystemTime>,
        _: Option<SystemTime>,
        bkuptime: Option<SystemTime>,
        flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        if uid.is_some_and(|uid| uid != self.binding.identity.uid)
            || gid.is_some_and(|gid| gid != self.binding.identity.gid)
            || crtime.is_some()
            || bkuptime.is_some()
            || flags.is_some_and(|flags| !flags.is_empty())
        {
            reply.error(Errno::EOPNOTSUPP);
            return;
        }
        let modified_ns = mtime.map(|time| {
            time_ns(match time {
                TimeOrNow::SpecificTime(time) => time,
                TimeOrNow::Now => SystemTime::now(),
            })
        });
        match self.runtime.block_on(self.files.setattr(
            &self.context(request),
            InodeId(inode.0),
            AttributeUpdate {
                mode,
                size,
                modified_ns,
            },
        )) {
            Ok(inode) => reply.attr(&Duration::ZERO, &attr(inode)),
            Err(error) => reply.error(errno(error)),
        }
    }
    fn mknod(
        &self,
        request: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _: u32,
        reply: ReplyEntry,
    ) {
        if mode & 0o170000 != 0o100000 {
            reply.error(Errno::EOPNOTSUPP);
            return;
        }
        let result = self.name(name).and_then(|name| {
            self.runtime.block_on(self.files.create(
                &self.context(request),
                InodeId(parent.0),
                name,
                FileKind::File,
                mode & !umask,
            ))
        });
        match result {
            Ok(inode) => reply.entry(
                &Duration::ZERO,
                &attr(inode),
                Generation(self.binding.generation),
            ),
            Err(error) => reply.error(errno(error)),
        }
    }
    fn mkdir(
        &self,
        request: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let result = self.name(name).and_then(|name| {
            self.runtime.block_on(self.files.create(
                &self.context(request),
                InodeId(parent.0),
                name,
                FileKind::Directory,
                mode & !umask,
            ))
        });
        match result {
            Ok(inode) => reply.entry(
                &Duration::ZERO,
                &attr(inode),
                Generation(self.binding.generation),
            ),
            Err(error) => reply.error(errno(error)),
        }
    }
    fn unlink(&self, request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.empty(
            self.name(name).and_then(|name| {
                self.runtime.block_on(self.files.unlink(
                    &self.context(request),
                    InodeId(parent.0),
                    name,
                    false,
                ))
            }),
            reply,
        );
    }
    fn rmdir(&self, request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.empty(
            self.name(name).and_then(|name| {
                self.runtime.block_on(self.files.unlink(
                    &self.context(request),
                    InodeId(parent.0),
                    name,
                    true,
                ))
            }),
            reply,
        );
    }
    fn rename(
        &self,
        request: &Request,
        parent: INodeNo,
        name: &OsStr,
        new_parent: INodeNo,
        new_name: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        if flags.bits() & !1 != 0 {
            reply.error(Errno::EOPNOTSUPP);
            return;
        }
        let result = (|| {
            self.runtime.block_on(self.files.rename(
                &self.context(request),
                InodeId(parent.0),
                self.name(name)?,
                InodeId(new_parent.0),
                self.name(new_name)?,
                flags.bits() & 1 == 0,
            ))
        })();
        self.empty(result, reply);
    }
    fn open(&self, request: &Request, inode: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        match self.open_file(&self.context(request), InodeId(inode.0), flags.0) {
            Ok(id) => reply.opened(fuser::FileHandle(id), FopenFlags::FOPEN_DIRECT_IO),
            Err(error) => reply.error(errno(error)),
        }
    }
    fn create(
        &self,
        request: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let context = self.context(request);
        let result = (|| {
            let name = self.name(name)?;
            let inode = match self.runtime.block_on(self.files.create(
                &context,
                InodeId(parent.0),
                name,
                FileKind::File,
                mode & !umask,
            )) {
                Ok(inode) => inode,
                Err(error)
                    if error.code == ErrorCode::AlreadyExists && flags & libc::O_EXCL == 0 =>
                {
                    self.runtime
                        .block_on(self.files.lookup(&context, InodeId(parent.0), name))?
                }
                Err(error) => return Err(error),
            };
            if flags & libc::O_TRUNC != 0 {
                self.runtime.block_on(self.files.setattr(
                    &context,
                    inode.id,
                    AttributeUpdate {
                        size: Some(0),
                        ..Default::default()
                    },
                ))?;
            }
            let handle = self.open_file(&context, inode.id, flags)?;
            Ok((inode, handle))
        })();
        match result {
            Ok((inode, handle)) => reply.created(
                &Duration::ZERO,
                &attr(inode),
                Generation(self.binding.generation),
                fuser::FileHandle(handle),
                FopenFlags::FOPEN_DIRECT_IO,
            ),
            Err(error) => reply.error(errno(error)),
        }
    }
    fn read(
        &self,
        request: &Request,
        _: INodeNo,
        handle: fuser::FileHandle,
        offset: u64,
        size: u32,
        _: OpenFlags,
        _: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let result = self.handle(handle).and_then(|handle| {
            self.runtime.block_on(
                self.files
                    .read(&self.context(request), handle, offset, size),
            )
        });
        match result {
            Ok(bytes) => reply.data(&bytes),
            Err(error) => reply.error(errno(error)),
        }
    }
    fn write(
        &self,
        request: &Request,
        _: INodeNo,
        handle: fuser::FileHandle,
        offset: u64,
        data: &[u8],
        _: WriteFlags,
        flags: OpenFlags,
        _: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let context = self.context(request);
        let result = (|| {
            let written = self.runtime.block_on(self.files.write(
                &context,
                self.handle(handle)?,
                offset,
                Bytes::copy_from_slice(data),
            ))?;
            if flags.0 & (libc::O_SYNC | libc::O_DSYNC) != 0 {
                self.runtime.block_on(self.files.fsync(&context))?;
            }
            Ok(written)
        })();
        match result {
            Ok(written) => reply.written(written),
            Err(error) => reply.error(errno(error)),
        }
    }
    fn flush(
        &self,
        _: &Request,
        _: INodeNo,
        handle: fuser::FileHandle,
        _: LockOwner,
        reply: ReplyEmpty,
    ) {
        self.empty(self.handle(handle).map(|_| ()), reply);
    }
    fn release(
        &self,
        request: &Request,
        _: INodeNo,
        handle: fuser::FileHandle,
        _: OpenFlags,
        _: Option<LockOwner>,
        _: bool,
        reply: ReplyEmpty,
    ) {
        let result = self
            .handles
            .lock()
            .remove(&handle.0)
            .ok_or_else(|| Error::new(ErrorCode::StaleBinding, "native file handle is closed"))
            .and_then(|handle| {
                self.runtime
                    .block_on(self.files.release(&self.context(request), handle))
            });
        self.empty(result, reply);
    }
    fn fsync(
        &self,
        request: &Request,
        _: INodeNo,
        _: fuser::FileHandle,
        _: bool,
        reply: ReplyEmpty,
    ) {
        self.empty(
            self.runtime
                .block_on(self.files.fsync(&self.context(request)))
                .map(|_| ()),
            reply,
        );
    }
    fn opendir(&self, request: &Request, inode: INodeNo, _: OpenFlags, reply: ReplyOpen) {
        let context = self.context(request);
        let result = (|| {
            let entries = self
                .runtime
                .block_on(self.files.readdir(&context, InodeId(inode.0)))?;
            let current = self
                .runtime
                .block_on(self.files.getattr(&context, InodeId(inode.0)))?;
            let parent =
                self.runtime
                    .block_on(self.files.lookup(&context, InodeId(inode.0), ".."))?;
            let mut snapshot = vec![(".".into(), current), ("..".into(), parent)];
            snapshot.extend(entries.into_iter().map(|entry| (entry.name, entry.inode)));
            let id = self.next()?;
            self.directories.lock().insert(id, snapshot);
            Ok(id)
        })();
        match result {
            Ok(id) => reply.opened(fuser::FileHandle(id), FopenFlags::empty()),
            Err(error) => reply.error(errno(error)),
        }
    }
    fn readdir(
        &self,
        request: &Request,
        inode: INodeNo,
        handle: fuser::FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        if let Err(error) = self
            .runtime
            .block_on(self.files.getattr(&self.context(request), InodeId(inode.0)))
        {
            reply.error(errno(error));
            return;
        }
        let directories = self.directories.lock();
        let Some(entries) = directories.get(&handle.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        for (index, (name, inode)) in entries
            .iter()
            .enumerate()
            .skip(usize::try_from(offset).unwrap_or(usize::MAX))
        {
            if reply.add(
                INodeNo(inode.id.0),
                (index + 1) as u64,
                file_type(inode.kind),
                name,
            ) {
                break;
            }
        }
        reply.ok();
    }
    fn releasedir(
        &self,
        _: &Request,
        _: INodeNo,
        handle: fuser::FileHandle,
        _: OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.directories.lock().remove(&handle.0);
        reply.ok();
    }
    fn fsyncdir(
        &self,
        request: &Request,
        _: INodeNo,
        _: fuser::FileHandle,
        _: bool,
        reply: ReplyEmpty,
    ) {
        self.empty(
            self.runtime
                .block_on(self.files.fsync(&self.context(request)))
                .map(|_| ()),
            reply,
        );
    }
    fn statfs(&self, request: &Request, _: INodeNo, reply: ReplyStatfs) {
        match self
            .runtime
            .block_on(self.files.statfs(&self.context(request)))
        {
            Ok(stats) => reply.statfs(
                stats.total_bytes / 4096,
                stats.available_bytes / 4096,
                stats.available_bytes / 4096,
                1_000_000,
                1_000_000,
                4096,
                255,
                4096,
            ),
            Err(error) => reply.error(errno(error)),
        }
    }
}
