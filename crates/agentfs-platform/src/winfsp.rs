use agentfs_model::*;
use agentfs_ports::{AttributeUpdate, FileSystem, MountSpec, NativeMountState};
use bytes::Bytes;
use parking_lot::Mutex;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::runtime::Handle;
use winfsp_wrs::{
    self as win, CleanupFlags, CreateFileInfo, CreateOptions, DirInfo, FileAccessRights,
    FileAttributes, FileInfo, FileSystemInterface, PSecurityDescriptor, SecurityDescriptor,
    U16CStr, U16CString, VolumeInfo, WriteMode,
};

type NtResult<T> = std::result::Result<T, i32>;
struct Mounted {
    binding: MountBinding,
    filesystem: Option<win::FileSystem>,
    open: Arc<AtomicUsize>,
    alive: Arc<AtomicBool>,
}
impl Drop for Mounted {
    fn drop(&mut self) {
        if let Some(filesystem) = self.filesystem.take() {
            filesystem.stop();
        }
    }
}
pub(super) struct Driver {
    mounts: Arc<Mutex<BTreeMap<MountId, Mounted>>>,
}
impl Driver {
    pub fn new() -> Result<Self> {
        Ok(Self {
            mounts: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }
    pub async fn mount(&self, spec: MountSpec, files: Arc<dyn FileSystem>) -> Result<()> {
        let runtime = Handle::current();
        let mounts = self.mounts.clone();
        tokio::task::spawn_blocking(move || {
            win::init().map_err(|error| Error::new(ErrorCode::Unsupported, error.to_string()))?;
            let binding = spec.binding;
            if binding.path.exists() && std::fs::read_dir(&binding.path)?.next().is_some() {
                return Err(Error::new(
                    ErrorCode::MountBusy,
                    "mount directory must be empty",
                ));
            }
            let point = U16CString::from_os_str(&binding.path)
                .map_err(|_| Error::invalid("invalid Windows mount path"))?;
            let mut volume = win::VolumeParams::default();
            volume
                .set_sector_size(512)
                .set_sectors_per_allocation_unit(8)
                .set_max_component_length(255)
                .set_case_sensitive_search(false)
                .set_case_preserved_names(true)
                .set_unicode_on_disk(true)
                .set_persistent_acls(false)
                .set_read_only_volume(binding.access == AccessMode::ReadOnly)
                .set_file_info_timeout(0)
                .set_dir_info_timeout(0)
                .set_volume_info_timeout(0)
                .set_flush_and_purge_on_cleanup(true);
            volume
                .set_file_system_name(win::u16cstr!("AgentFS"))
                .map_err(|_| Error::invalid("filesystem name exceeds WinFsp limit"))?;
            let open = Arc::new(AtomicUsize::new(0));
            let alive = Arc::new(AtomicBool::new(true));
            let filesystem = WindowsFs {
                binding: binding.clone(),
                files,
                runtime,
                descriptor: security::owner_descriptor()?,
                open: open.clone(),
                alive: alive.clone(),
            };
            let filesystem = win::FileSystem::start(
                win::Params {
                    volume_params: volume,
                    guard_strategy: win::OperationGuardStrategy::Coarse,
                },
                Some(&point),
                filesystem,
            )
            .map_err(native_error)?;
            mounts.lock().insert(
                binding.id,
                Mounted {
                    binding,
                    filesystem: Some(filesystem),
                    open,
                    alive,
                },
            );
            Ok(())
        })
        .await
        .map_err(|error| Error::new(ErrorCode::Io, error.to_string()))?
    }
    pub async fn status(&self, binding: &MountBinding) -> Result<NativeMountState> {
        if let Some(current) = self.mounts.lock().get(&binding.id)
            && current.binding.generation == binding.generation
            && current.alive.load(Ordering::Acquire)
        {
            return Ok(NativeMountState::Mounted);
        }
        match std::fs::symlink_metadata(&binding.path) {
            Ok(metadata) => {
                use std::os::windows::fs::MetadataExt;
                Ok(if metadata.file_attributes() & 0x400 != 0 {
                    NativeMountState::Unknown
                } else {
                    NativeMountState::Absent
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(NativeMountState::Absent)
            }
            Err(error) => Err(error.into()),
        }
    }
    pub async fn unmount(&self, binding: &MountBinding) -> Result<()> {
        let current = {
            let mut mounts = self.mounts.lock();
            if let Some(current) = mounts.get(&binding.id) {
                if current.binding.generation != binding.generation {
                    return Err(Error::new(
                        ErrorCode::StaleBinding,
                        "mount generation changed",
                    ));
                }
                if current.open.load(Ordering::Acquire) != 0 {
                    return Err(Error::new(
                        ErrorCode::MountBusy,
                        "native mount still has open handles",
                    ));
                }
            }
            mounts.remove(&binding.id)
        };
        if let Some(current) = current {
            tokio::task::spawn_blocking(move || drop(current))
                .await
                .map_err(|error| Error::new(ErrorCode::Io, error.to_string()))?;
        } else if self.status(binding).await? != NativeMountState::Absent {
            return Err(Error::new(
                ErrorCode::MountBusy,
                "mount path belongs to another filesystem",
            ));
        }
        Ok(())
    }
}
fn native_error(status: i32) -> Error {
    Error::new(ErrorCode::Io, format!("WinFsp status {status:#x}"))
}
fn nt(error: Error) -> i32 {
    match error.code {
        ErrorCode::NotFound => win::STATUS_OBJECT_NAME_NOT_FOUND,
        ErrorCode::AlreadyExists | ErrorCode::NameConflict => win::STATUS_OBJECT_NAME_COLLISION,
        ErrorCode::NotDirectory => win::STATUS_NOT_A_DIRECTORY,
        ErrorCode::IsDirectory => win::STATUS_FILE_IS_A_DIRECTORY,
        ErrorCode::DirectoryNotEmpty => win::STATUS_DIRECTORY_NOT_EMPTY,
        ErrorCode::PermissionDenied => win::STATUS_ACCESS_DENIED,
        ErrorCode::ReadOnly => win::STATUS_MEDIA_WRITE_PROTECTED,
        ErrorCode::Busy | ErrorCode::MountBusy | ErrorCode::BranchBusy => {
            win::STATUS_SHARING_VIOLATION
        }
        ErrorCode::InvalidArgument => win::STATUS_INVALID_PARAMETER,
        ErrorCode::Unsupported => win::STATUS_NOT_SUPPORTED,
        ErrorCode::CapacityExceeded => win::STATUS_DISK_FULL,
        ErrorCode::StaleBinding => win::STATUS_FILE_INVALID,
        _ => win::STATUS_IO_DEVICE_ERROR,
    }
}
fn filetime(ns: i64) -> u64 {
    ((i128::from(ns) / 100) + 116_444_736_000_000_000).clamp(0, i128::from(u64::MAX)) as u64
}
fn nanoseconds(time: u64) -> i64 {
    ((i128::from(time) - 116_444_736_000_000_000) * 100)
        .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}
fn info(inode: Inode) -> FileInfo {
    let mut result = FileInfo::default();
    let mut attributes = if inode.kind == FileKind::Directory {
        FileAttributes::DIRECTORY
    } else {
        FileAttributes::NORMAL
    };
    if inode.mode & 0o200 == 0 {
        attributes |= FileAttributes::READONLY;
    }
    result
        .set_file_attributes(attributes)
        .set_file_size(inode.size)
        .set_allocation_size(inode.size.div_ceil(4096) * 4096)
        .set_creation_time(filetime(inode.created_ns))
        .set_last_write_time(filetime(inode.modified_ns))
        .set_last_access_time(filetime(inode.modified_ns))
        .set_change_time(filetime(inode.modified_ns))
        .set_index_number(inode.id.0)
        .set_hard_links(1);
    result
}
struct OpenFile {
    inode: InodeId,
    handle: Mutex<Option<HandleId>>,
    directory: bool,
    write_through: bool,
    entries: Mutex<Option<Vec<(String, Inode)>>>,
}
struct WindowsFs {
    binding: MountBinding,
    files: Arc<dyn FileSystem>,
    runtime: Handle,
    descriptor: SecurityDescriptor,
    open: Arc<AtomicUsize>,
    alive: Arc<AtomicBool>,
}
impl WindowsFs {
    fn context(&self) -> FsContext {
        FsContext {
            mount: self.binding.id,
            generation: self.binding.generation,
            identity: self.binding.identity,
        }
    }
    fn path(&self, name: &U16CStr) -> Result<WorkspacePath> {
        name.to_string()
            .map_err(|_| Error::invalid("Windows path must be valid Unicode"))?
            .replace('\\', "/")
            .try_into()
    }
    fn lookup(&self, path: &WorkspacePath) -> Result<Inode> {
        let mut inode = self
            .runtime
            .block_on(self.files.getattr(&self.context(), InodeId::ROOT))?;
        for name in path.as_str().split('/').filter(|name| !name.is_empty()) {
            inode = self
                .runtime
                .block_on(self.files.lookup(&self.context(), inode.id, name))?;
        }
        Ok(inode)
    }
    fn parent(&self, path: &WorkspacePath) -> Result<Inode> {
        self.lookup(&path.parent().ok_or_else(|| {
            Error::new(
                ErrorCode::PermissionDenied,
                "filesystem root cannot be changed",
            )
        })?)
    }
    fn opened(
        &self,
        inode: Inode,
        options: CreateOptions,
        access: FileAccessRights,
    ) -> Result<(Arc<OpenFile>, FileInfo)> {
        let directory = inode.kind == FileKind::Directory;
        if options.is(CreateOptions::FILE_DIRECTORY_FILE) && !directory {
            return Err(Error::new(ErrorCode::NotDirectory, "expected directory"));
        }
        if options.is(CreateOptions::FILE_NON_DIRECTORY_FILE) && directory {
            return Err(Error::new(ErrorCode::IsDirectory, "expected file"));
        }
        let read = access.is(FileAccessRights::FILE_READ_DATA);
        let write = access.is(FileAccessRights::FILE_WRITE_DATA)
            || access.is(FileAccessRights::FILE_APPEND_DATA);
        let handle = if !directory && (read || write) {
            Some(
                self.runtime
                    .block_on(self.files.open(
                        &self.context(),
                        inode.id,
                        OpenMode {
                            read,
                            write,
                            append: write && !access.is(FileAccessRights::FILE_WRITE_DATA),
                        },
                    ))?
                    .id,
            )
        } else {
            None
        };
        self.open.fetch_add(1, Ordering::Release);
        Ok((
            Arc::new(OpenFile {
                inode: inode.id,
                handle: Mutex::new(handle),
                directory,
                write_through: options.is(CreateOptions::FILE_WRITE_THROUGH),
                entries: Mutex::new(None),
            }),
            info(inode),
        ))
    }
    fn handle(&self, file: &OpenFile) -> Result<HandleId> {
        (*file.handle.lock()).ok_or_else(|| {
            Error::new(
                ErrorCode::PermissionDenied,
                "file handle does not allow data access",
            )
        })
    }
    fn release(&self, file: &OpenFile) {
        if let Some(handle) = file.handle.lock().take()
            && let Err(error) = self
                .runtime
                .block_on(self.files.release(&self.context(), handle))
        {
            tracing::warn!(%error, "cannot release Windows file handle");
        }
    }
}
impl FileSystemInterface for WindowsFs {
    type FileContext = Arc<OpenFile>;
    const GET_VOLUME_INFO_DEFINED: bool = true;
    const GET_SECURITY_BY_NAME_DEFINED: bool = true;
    const CREATE_DEFINED: bool = true;
    const OPEN_DEFINED: bool = true;
    const OVERWRITE_DEFINED: bool = true;
    const CLEANUP_DEFINED: bool = true;
    const CLOSE_DEFINED: bool = true;
    const READ_DEFINED: bool = true;
    const WRITE_DEFINED: bool = true;
    const FLUSH_DEFINED: bool = true;
    const GET_FILE_INFO_DEFINED: bool = true;
    const SET_BASIC_INFO_DEFINED: bool = true;
    const SET_FILE_SIZE_DEFINED: bool = true;
    const CAN_DELETE_DEFINED: bool = true;
    const RENAME_DEFINED: bool = true;
    const GET_SECURITY_DEFINED: bool = true;
    const READ_DIRECTORY_DEFINED: bool = true;
    const DISPATCHER_STOPPED_DEFINED: bool = true;
    fn get_volume_info(&self) -> NtResult<VolumeInfo> {
        let stats = self
            .runtime
            .block_on(self.files.statfs(&self.context()))
            .map_err(nt)?;
        VolumeInfo::new(
            stats.total_bytes,
            stats.available_bytes,
            win::u16str!("AgentFS"),
        )
        .map_err(|_| win::STATUS_INVALID_PARAMETER)
    }
    fn get_security_by_name(
        &self,
        name: &U16CStr,
        _: impl Fn() -> Option<FileAttributes>,
    ) -> NtResult<(FileAttributes, PSecurityDescriptor, bool)> {
        let inode = self.lookup(&self.path(name).map_err(nt)?).map_err(nt)?;
        Ok((
            info(inode).file_attributes(),
            self.descriptor.as_ptr(),
            false,
        ))
    }
    fn create(
        &self,
        name: &U16CStr,
        create: CreateFileInfo,
        _: SecurityDescriptor,
    ) -> NtResult<(Arc<OpenFile>, FileInfo)> {
        let path = self.path(name).map_err(nt)?;
        let parent = self.parent(&path).map_err(nt)?;
        let directory = create.create_options.is(CreateOptions::FILE_DIRECTORY_FILE);
        let mode = if directory { 0o755 } else { 0o644 };
        let inode = self
            .runtime
            .block_on(self.files.create(
                &self.context(),
                parent.id,
                path.name(),
                if directory {
                    FileKind::Directory
                } else {
                    FileKind::File
                },
                mode,
            ))
            .map_err(nt)?;
        let id = inode.id;
        let (file, mut info) = self
            .opened(inode, create.create_options, create.granted_access)
            .map_err(nt)?;
        if !directory && create.file_attributes.is(FileAttributes::READONLY) {
            match self.runtime.block_on(self.files.setattr(
                &self.context(),
                id,
                AttributeUpdate {
                    mode: Some(0o444),
                    ..Default::default()
                },
            )) {
                Ok(inode) => info = self::info(inode),
                Err(error) => {
                    self.close(file);
                    return Err(nt(error));
                }
            }
        }
        Ok((file, info))
    }
    fn open(
        &self,
        name: &U16CStr,
        options: CreateOptions,
        access: FileAccessRights,
    ) -> NtResult<(Arc<OpenFile>, FileInfo)> {
        self.opened(
            self.lookup(&self.path(name).map_err(nt)?).map_err(nt)?,
            options,
            access,
        )
        .map_err(nt)
    }
    fn overwrite(
        &self,
        file: Arc<OpenFile>,
        attributes: FileAttributes,
        replace: bool,
        _: u64,
    ) -> NtResult<FileInfo> {
        self.runtime
            .block_on(self.files.setattr(
                &self.context(),
                file.inode,
                AttributeUpdate {
                    size: Some(0),
                    mode: replace.then_some(if attributes.is(FileAttributes::READONLY) {
                        0o444
                    } else {
                        0o644
                    }),
                    modified_ns: None,
                },
            ))
            .map(info)
            .map_err(nt)
    }
    fn cleanup(&self, file: Arc<OpenFile>, name: Option<&U16CStr>, flags: CleanupFlags) {
        if flags.is(CleanupFlags::DELETE) {
            self.release(&file);
            let result = (|| {
                let path =
                    self.path(name.ok_or_else(|| Error::invalid("delete requires a file name"))?)?;
                let parent = self.parent(&path)?;
                self.runtime.block_on(self.files.unlink(
                    &self.context(),
                    parent.id,
                    path.name(),
                    file.directory,
                ))
            })();
            if let Err(error) = result {
                tracing::error!(%error, "Windows cleanup could not delete file");
            }
        }
    }
    fn close(&self, file: Arc<OpenFile>) {
        self.release(&file);
        self.open.fetch_sub(1, Ordering::Release);
    }
    fn read(&self, file: Arc<OpenFile>, buffer: &mut [u8], offset: u64) -> NtResult<usize> {
        let size = buffer.len().min(IO_BUFFER_BYTES * 16) as u32;
        let bytes = self
            .runtime
            .block_on(self.files.read(
                &self.context(),
                self.handle(&file).map_err(nt)?,
                offset,
                size,
            ))
            .map_err(nt)?;
        if bytes.is_empty() && !buffer.is_empty() {
            return Err(win::STATUS_END_OF_FILE);
        }
        buffer[..bytes.len()].copy_from_slice(&bytes);
        Ok(bytes.len())
    }
    fn write(
        &self,
        file: Arc<OpenFile>,
        buffer: &[u8],
        mode: WriteMode,
    ) -> NtResult<(usize, FileInfo)> {
        let context = self.context();
        let buffer = &buffer[..buffer.len().min(IO_BUFFER_BYTES * 16)];
        let written = match mode {
            WriteMode::WriteToEOF => {
                self.handle(&file).map_err(nt)?;
                let handle = self
                    .runtime
                    .block_on(self.files.open(
                        &context,
                        file.inode,
                        OpenMode {
                            read: false,
                            write: true,
                            append: true,
                        },
                    ))
                    .map_err(nt)?;
                let result = self.runtime.block_on(self.files.write(
                    &context,
                    handle.id,
                    0,
                    Bytes::copy_from_slice(buffer),
                ));
                let released = self
                    .runtime
                    .block_on(self.files.release(&context, handle.id));
                let written = result.map_err(nt)?;
                released.map_err(nt)?;
                written
            }
            WriteMode::Normal { offset } => self
                .runtime
                .block_on(self.files.write(
                    &context,
                    self.handle(&file).map_err(nt)?,
                    offset,
                    Bytes::copy_from_slice(buffer),
                ))
                .map_err(nt)?,
            WriteMode::ConstrainedIO { offset } => {
                let inode = self
                    .runtime
                    .block_on(self.files.getattr(&context, file.inode))
                    .map_err(nt)?;
                let count = inode.size.saturating_sub(offset).min(buffer.len() as u64) as usize;
                self.runtime
                    .block_on(self.files.write(
                        &context,
                        self.handle(&file).map_err(nt)?,
                        offset,
                        Bytes::copy_from_slice(&buffer[..count]),
                    ))
                    .map_err(nt)?
            }
        };
        if file.write_through {
            self.runtime
                .block_on(self.files.fsync(&context))
                .map_err(nt)?;
        }
        Ok((written as usize, self.get_file_info(file)?))
    }
    fn flush(&self, file: Option<Arc<OpenFile>>) -> NtResult<FileInfo> {
        if self.binding.access == AccessMode::ReadWrite {
            self.runtime
                .block_on(self.files.fsync(&self.context()))
                .map_err(nt)?;
        }
        match file {
            Some(file) => self.get_file_info(file),
            None => Ok(FileInfo::default()),
        }
    }
    fn get_file_info(&self, file: Arc<OpenFile>) -> NtResult<FileInfo> {
        self.runtime
            .block_on(self.files.getattr(&self.context(), file.inode))
            .map(info)
            .map_err(nt)
    }
    fn set_basic_info(
        &self,
        file: Arc<OpenFile>,
        attributes: FileAttributes,
        _: u64,
        _: u64,
        modified: u64,
        _: u64,
    ) -> NtResult<FileInfo> {
        let inode = self
            .runtime
            .block_on(self.files.getattr(&self.context(), file.inode))
            .map_err(nt)?;
        let mode =
            (attributes.0 != u32::MAX).then_some(if attributes.is(FileAttributes::READONLY) {
                inode.mode & !0o222
            } else {
                inode.mode | 0o200
            });
        self.runtime
            .block_on(self.files.setattr(
                &self.context(),
                file.inode,
                AttributeUpdate {
                    mode,
                    size: None,
                    modified_ns: (modified != 0).then_some(nanoseconds(modified)),
                },
            ))
            .map(info)
            .map_err(nt)
    }
    fn set_file_size(
        &self,
        file: Arc<OpenFile>,
        size: u64,
        allocation: bool,
    ) -> NtResult<FileInfo> {
        let inode = self
            .runtime
            .block_on(self.files.getattr(&self.context(), file.inode))
            .map_err(nt)?;
        if allocation && size >= inode.size {
            return Ok(info(inode));
        }
        self.runtime
            .block_on(self.files.setattr(
                &self.context(),
                file.inode,
                AttributeUpdate {
                    size: Some(size),
                    ..Default::default()
                },
            ))
            .map(info)
            .map_err(nt)
    }
    fn can_delete(&self, file: Arc<OpenFile>, name: &U16CStr) -> NtResult<()> {
        let path = self.path(name).map_err(nt)?;
        let parent = self.parent(&path).map_err(nt)?;
        self.runtime
            .block_on(self.files.can_unlink(
                &self.context(),
                parent.id,
                path.name(),
                file.directory,
                *file.handle.lock(),
            ))
            .map_err(nt)
    }
    fn rename(
        &self,
        _: Arc<OpenFile>,
        name: &U16CStr,
        target: &U16CStr,
        replace: bool,
    ) -> NtResult<()> {
        let path = self.path(name).map_err(nt)?;
        let target = self.path(target).map_err(nt)?;
        self.runtime
            .block_on(self.files.rename(
                &self.context(),
                self.parent(&path).map_err(nt)?.id,
                path.name(),
                self.parent(&target).map_err(nt)?.id,
                target.name(),
                replace,
            ))
            .map_err(nt)
    }
    fn get_security(&self, _: Arc<OpenFile>) -> NtResult<PSecurityDescriptor> {
        Ok(self.descriptor.as_ptr())
    }
    fn read_directory(
        &self,
        file: Arc<OpenFile>,
        marker: Option<&U16CStr>,
        mut add: impl FnMut(DirInfo) -> bool,
    ) -> NtResult<()> {
        let mut snapshot = file.entries.lock();
        if marker.is_none() || snapshot.is_none() {
            let context = self.context();
            let entries = self
                .runtime
                .block_on(self.files.readdir(&context, file.inode))
                .map_err(nt)?;
            let current = self
                .runtime
                .block_on(self.files.getattr(&context, file.inode))
                .map_err(nt)?;
            let parent = self
                .runtime
                .block_on(self.files.lookup(&context, file.inode, ".."))
                .map_err(nt)?;
            let mut entries: Vec<_> = entries
                .into_iter()
                .map(|entry| (entry.name, entry.inode))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            if file.inode != InodeId::ROOT {
                entries.insert(0, ("..".into(), parent));
                entries.insert(0, (".".into(), current));
            }
            *snapshot = Some(entries);
        }
        let entries = snapshot.as_ref().ok_or(win::STATUS_IO_DEVICE_ERROR)?;
        let start = match marker {
            Some(marker) => {
                let marker = marker
                    .to_string()
                    .map_err(|_| win::STATUS_INVALID_PARAMETER)?;
                entries
                    .iter()
                    .position(|(name, _)| name == &marker)
                    .map(|index| index + 1)
                    .ok_or(win::STATUS_INVALID_PARAMETER)?
            }
            None => 0,
        };
        for (name, inode) in &entries[start..] {
            if !add(DirInfo::from_str(info(inode.clone()), name)) {
                break;
            }
        }
        Ok(())
    }
    fn dispatcher_stopped(&self, _: bool) {
        self.alive.store(false, Ordering::Release);
    }
}

#[allow(unsafe_code)]
mod security {
    use super::*;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::ConvertSidToStringSidW, GetTokenInformation, TOKEN_QUERY, TOKEN_USER,
            TokenUser,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };
    pub fn owner_descriptor() -> Result<SecurityDescriptor> {
        let mut token = std::ptr::null_mut();
        // SAFETY: The output pointer is valid; the resulting owned handle is closed by RAII.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: OpenProcessToken returned a valid owned handle.
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut size = 0;
        // SAFETY: A null output buffer requests the size only.
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                std::ptr::null_mut(),
                0,
                &mut size,
            );
        }
        if size == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut buffer = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        // SAFETY: The aligned allocation has at least size bytes and lives through the conversion.
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                buffer.as_mut_ptr().cast(),
                size,
                &mut size,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: Successful TokenUser output starts with TOKEN_USER and uses pointer alignment.
        let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
        let mut sid = std::ptr::null_mut();
        // SAFETY: The SID references the live token buffer and the output pointer is valid.
        if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: ConvertSidToStringSidW returns a NUL-terminated UTF-16 string.
        let value = unsafe { U16CStr::from_ptr_str(sid) }.to_string_lossy();
        // SAFETY: The SID string was allocated by the Windows API with LocalAlloc.
        unsafe {
            LocalFree(sid.cast());
        }
        let sddl = U16CString::from_str(format!(
            "O:{value}G:{value}D:P(A;;FA;;;{value})(A;;FA;;;SY)"
        ))
        .map_err(|_| Error::integrity("invalid SID string"))?;
        SecurityDescriptor::from_wstr(&sddl).map_err(|error| Error::new(ErrorCode::Io, error))
    }
}
