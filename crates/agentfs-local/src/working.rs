use crate::{LocalBackend, create_private_dir};
use agentfs_model::*;
use agentfs_ports::*;
use async_trait::async_trait;
use bytes::Bytes;
use std::{
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

impl LocalBackend {
    pub(crate) fn working_path(&self, file: WorkingFile) -> PathBuf {
        self.inner
            .root
            .join("working")
            .join(file.workspace.to_string())
            .join(file.branch.to_string())
            .join(file.inode.0.to_string())
    }
}

#[async_trait]
impl WorkingTreeStore for LocalBackend {
    async fn create(&self, file: WorkingFile, source: Option<ObjectStream>) -> Result<()> {
        let (temporary_file, temporary) = self
            .blocking(|backend| {
                Ok(
                    tempfile::NamedTempFile::new_in(backend.inner.root.join("staging"))?
                        .into_parts(),
                )
            })
            .await?;
        let mut output = tokio::fs::File::from_std(temporary_file);
        if let Some(mut stream) = source {
            let mut buffer = vec![0; IO_BUFFER_BYTES];
            let mut copied = 0u64;
            loop {
                let count = stream.reader.read(&mut buffer).await?;
                if count == 0 {
                    break;
                }
                copied = copied
                    .checked_add(count as u64)
                    .ok_or_else(|| Error::invalid("working file is too large"))?;
                if copied > stream.reference.size {
                    return Err(Error::integrity("object exceeded its declared size"));
                }
                output.write_all(&buffer[..count]).await?;
            }
            if copied != stream.reference.size {
                return Err(Error::integrity("object stream ended early"));
            }
        }
        output.flush().await?;
        drop(output);
        self.blocking(move |backend| {
            let destination = backend.working_path(file);
            create_private_dir(
                destination
                    .parent()
                    .ok_or_else(|| Error::integrity("working file has no parent"))?,
            )?;
            temporary
                .persist(destination)
                .map_err(|error| Error::from(error.error))?;
            Ok(())
        })
        .await
    }

    async fn read(&self, file: WorkingFile, offset: u64, size: u32) -> Result<Bytes> {
        self.blocking(move |backend| {
            let mut input = std::fs::File::open(backend.working_path(file))?;
            input.seek(SeekFrom::Start(offset))?;
            let mut data = vec![0; size as usize];
            let mut length = 0;
            while length < data.len() {
                let count = input.read(&mut data[length..])?;
                if count == 0 {
                    break;
                }
                length += count;
            }
            data.truncate(length);
            Ok(data.into())
        })
        .await
    }

    async fn write(&self, file: WorkingFile, offset: u64, data: Bytes) -> Result<u32> {
        let length =
            u32::try_from(data.len()).map_err(|_| Error::invalid("write buffer is too large"))?;
        self.blocking(move |backend| {
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .open(backend.working_path(file))?;
            output.seek(SeekFrom::Start(offset))?;
            output.write_all(&data)?;
            Ok(length)
        })
        .await
    }

    async fn truncate(&self, file: WorkingFile, size: u64) -> Result<()> {
        self.blocking(move |backend| {
            std::fs::OpenOptions::new()
                .write(true)
                .open(backend.working_path(file))?
                .set_len(size)?;
            Ok(())
        })
        .await
    }

    async fn remove(&self, file: WorkingFile) -> Result<()> {
        self.blocking(
            move |backend| match std::fs::remove_file(backend.working_path(file)) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            },
        )
        .await
    }

    async fn discard_branch(&self, workspace: WorkspaceId, branch: BranchId) -> Result<()> {
        self.blocking(move |backend| {
            let path = backend
                .inner
                .root
                .join("working")
                .join(workspace.to_string())
                .join(branch.to_string());
            match std::fs::remove_dir_all(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            }
        })
        .await
    }
}
