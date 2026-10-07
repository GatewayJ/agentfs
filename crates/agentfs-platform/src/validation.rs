use crate::directories::write_tree;
use agentfs_model::*;
use agentfs_ports::*;
use async_trait::async_trait;
use cap_std::{ambient_authority, fs::Dir};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

#[derive(Debug)]
pub struct ContainerValidation {
    docker: PathBuf,
}
impl ContainerValidation {
    pub fn new(docker: PathBuf) -> Self {
        Self { docker }
    }
}

async fn capture(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut result = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let retained = count.min(limit.saturating_sub(result.len()));
        result.extend_from_slice(&buffer[..retained]);
        truncated |= retained != count;
    }
    Ok((result, truncated))
}

#[async_trait]
impl ValidationRunner for ContainerValidation {
    async fn run(&self, request: ValidationRequest) -> Result<ValidationRecord> {
        let config = &request.config;
        if config.image.is_empty()
            || config.image.starts_with('-')
            || config.image.chars().any(char::is_whitespace)
            || config.timeout_seconds == 0
            || config.timeout_seconds > 3600
            || config.max_output_bytes == 0
            || config.max_output_bytes > MAX_METADATA_BYTES
        {
            return Err(Error::invalid("invalid validation limits or image"));
        }
        let temporary = tempfile::tempdir()?;
        let directory = Dir::open_ambient_dir(temporary.path(), ambient_authority())?;
        write_tree(
            &directory,
            request.workspace,
            &request.tree,
            request.content,
        )
        .await?;
        let source = temporary
            .path()
            .to_str()
            .ok_or_else(|| Error::invalid("validation directory must be UTF-8"))?;
        if source.contains(',') {
            return Err(Error::invalid(
                "validation directory contains an unsupported comma",
            ));
        }
        let name = format!("agentfs-validate-{}", uuid::Uuid::new_v4());
        let mut child = Command::new(&self.docker)
            .args([
                "run",
                "--rm",
                "--pull=never",
                "--name",
                &name,
                "--network=none",
                "--read-only",
                "--cap-drop=ALL",
                "--security-opt=no-new-privileges",
                "--pids-limit=128",
                "--memory=512m",
                "--cpus=1",
                "--user=65534:65534",
                "--tmpfs=/tmp:rw,nosuid,nodev,size=128m",
                "--workdir=/workspace",
                "--mount",
            ])
            .arg(format!(
                "type=bind,source={source},target=/workspace,readonly"
            ))
            .arg(&config.image)
            .arg(&config.program)
            .args(&config.arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::integrity("validation stdout was not captured"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::integrity("validation stderr was not captured"))?;
        let limit = config.max_output_bytes / 2;
        let output_task = tokio::spawn(capture(stdout, limit));
        let error_task = tokio::spawn(capture(stderr, limit));
        let status =
            match tokio::time::timeout(Duration::from_secs(config.timeout_seconds), child.wait())
                .await
            {
                Ok(status) => Some(status?),
                Err(_) => {
                    let _ = tokio::time::timeout(
                        Duration::from_secs(15),
                        Command::new(&self.docker)
                            .args(["rm", "--force", &name])
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status(),
                    )
                    .await;
                    let _ = child.kill().await;
                    None
                }
            };
        let (stdout, out_truncated) = output_task
            .await
            .map_err(|error| Error::new(ErrorCode::Io, error.to_string()))??;
        let (stderr, err_truncated) = error_task
            .await
            .map_err(|error| Error::new(ErrorCode::Io, error.to_string()))??;
        let mut output = format!(
            "{}{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
        if status.is_none() {
            output.push_str("\nValidation exceeded its time limit.");
        }
        if out_truncated || err_truncated {
            output.push_str("\nValidation exceeded its output limit.");
        }
        Ok(ValidationRecord {
            candidate: request.candidate,
            config_version: object_ref(ObjectKind::RecordIndex, &encode(config)?).id,
            passed: status.is_some_and(|status| status.success())
                && !out_truncated
                && !err_truncated,
            skipped: false,
            output,
        })
    }
}
