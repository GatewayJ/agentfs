use agentfs_model::*;
use agentfs_ports::*;
use async_trait::async_trait;
use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
};

#[cfg(any(target_os = "linux", all(target_os = "macos", feature = "macos-mount")))]
#[path = "fuse.rs"]
mod implementation;
#[cfg(windows)]
#[path = "winfsp.rs"]
mod implementation;

pub struct NativeMountDriver {
    roots: Vec<PathBuf>,
    #[cfg(any(
        target_os = "linux",
        windows,
        all(target_os = "macos", feature = "macos-mount")
    ))]
    implementation: implementation::Driver,
}
impl std::fmt::Debug for NativeMountDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeMountDriver")
            .field("roots", &self.roots)
            .finish_non_exhaustive()
    }
}
impl NativeMountDriver {
    pub fn new(roots: Vec<PathBuf>) -> Result<Arc<Self>> {
        let mut canonical = Vec::new();
        for root in roots {
            std::fs::create_dir_all(&root)?;
            canonical.push(root.canonicalize()?);
        }
        Ok(Arc::new(Self {
            roots: canonical,
            #[cfg(any(
                target_os = "linux",
                windows,
                all(target_os = "macos", feature = "macos-mount")
            ))]
            implementation: implementation::Driver::new()?,
        }))
    }
}

#[async_trait]
impl MountDriver for NativeMountDriver {
    async fn normalize_path(&self, path: &Path) -> Result<PathBuf> {
        if !path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
        {
            return Err(Error::invalid("mount path must be absolute and normalized"));
        }
        let parent = path
            .parent()
            .ok_or_else(|| Error::invalid("cannot mount over a filesystem root"))?
            .canonicalize()?;
        let name = path
            .file_name()
            .ok_or_else(|| Error::invalid("mount point needs a directory name"))?;
        let path = parent.join(name);
        if !self
            .roots
            .iter()
            .any(|root| path.starts_with(root) && &path != root)
        {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "mount path is outside the configured mount roots",
            ));
        }
        if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "mount path must not be a symbolic link",
            ));
        }
        Ok(path)
    }

    async fn mount(&self, spec: MountSpec, file_system: Arc<dyn FileSystem>) -> Result<()> {
        self.normalize_path(&spec.binding.path).await?;
        #[cfg(any(
            target_os = "linux",
            windows,
            all(target_os = "macos", feature = "macos-mount")
        ))]
        {
            self.implementation.mount(spec, file_system).await
        }
        #[cfg(not(any(
            target_os = "linux",
            windows,
            all(target_os = "macos", feature = "macos-mount")
        )))]
        {
            let _ = file_system;
            Err(Error::new(
                ErrorCode::Unsupported,
                "native mounts require macFUSE and a build with the macos-mount feature",
            ))
        }
    }

    async fn unmount(&self, binding: &MountBinding) -> Result<()> {
        #[cfg(any(
            target_os = "linux",
            windows,
            all(target_os = "macos", feature = "macos-mount")
        ))]
        {
            self.implementation.unmount(binding).await
        }
        #[cfg(not(any(
            target_os = "linux",
            windows,
            all(target_os = "macos", feature = "macos-mount")
        )))]
        {
            let _ = binding;
            Err(Error::new(
                ErrorCode::Unsupported,
                "native mount support is not enabled",
            ))
        }
    }

    async fn status(&self, binding: &MountBinding) -> Result<NativeMountState> {
        #[cfg(any(
            target_os = "linux",
            windows,
            all(target_os = "macos", feature = "macos-mount")
        ))]
        {
            self.implementation.status(binding).await
        }
        #[cfg(not(any(
            target_os = "linux",
            windows,
            all(target_os = "macos", feature = "macos-mount")
        )))]
        {
            let _ = binding;
            Ok(NativeMountState::Absent)
        }
    }

    fn capability(&self) -> &'static str {
        #[cfg(target_os = "linux")]
        {
            "linux-fuse"
        }
        #[cfg(windows)]
        {
            "windows-winfsp"
        }
        #[cfg(all(target_os = "macos", feature = "macos-mount"))]
        {
            "macos-macfuse"
        }
        #[cfg(all(target_os = "macos", not(feature = "macos-mount")))]
        {
            "macos-macfuse-feature-required"
        }
    }
}
