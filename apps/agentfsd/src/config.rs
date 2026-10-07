use agentfs_mcp::Credential;
use agentfs_model::*;
use agentfs_ports::ValidationConfig;
use agentfs_s3::S3Config;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub data_dir: PathBuf,
    pub listen: SocketAddr,
    pub mount_roots: Vec<PathBuf>,
    #[serde(default)]
    pub directory_roots: Vec<PathBuf>,
    pub credentials: Vec<TokenFile>,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub validation: BTreeMap<String, ValidationConfig>,
    #[serde(default = "docker")]
    pub docker: PathBuf,
    pub remote: Option<S3Config>,
}
fn docker() -> PathBuf {
    "docker".into()
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenFile {
    pub principal: Principal,
    pub token_file: PathBuf,
}
impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let mut bytes = Vec::new();
        fs::File::open(path)?
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 1024 * 1024 {
            return Err(Error::invalid("configuration exceeds 1 MiB"));
        }
        let config: Self =
            serde_json::from_slice(&bytes).map_err(|error| Error::invalid(error.to_string()))?;
        if !config.listen.ip().is_loopback() {
            return Err(Error::invalid(
                "MCP must listen on a loopback address; use an authenticated TLS proxy for remote access",
            ));
        }
        for path in std::iter::once(&config.data_dir)
            .chain(config.mount_roots.iter())
            .chain(config.directory_roots.iter())
            .chain(config.credentials.iter().map(|entry| &entry.token_file))
        {
            if !path.is_absolute() {
                return Err(Error::invalid(
                    "configured filesystem paths must be absolute",
                ));
            }
        }
        config.cache.validate()?;
        Ok(config)
    }
    pub fn credentials(&self) -> Result<Vec<Credential>> {
        self.credentials
            .iter()
            .map(|entry| {
                let metadata = fs::symlink_metadata(&entry.token_file)?;
                if !metadata.is_file() {
                    return Err(Error::invalid("token must be a regular file"));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if metadata.permissions().mode() & 0o077 != 0 {
                        return Err(Error::new(
                            ErrorCode::PermissionDenied,
                            "token file must be accessible only to its owner",
                        ));
                    }
                }
                if metadata.len() > 4096 {
                    return Err(Error::invalid("token file is too large"));
                }
                let token = fs::read_to_string(&entry.token_file)?.trim().to_owned();
                Ok(Credential {
                    principal: entry.principal.clone(),
                    token,
                })
            })
            .collect()
    }
}
fn create_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
pub fn initialize(directory: &Path, listen: SocketAddr) -> Result<PathBuf> {
    fs::create_dir_all(directory)?;
    let directory = directory.canonicalize()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }
    let config_path = directory.join("agentfs.json");
    if config_path.exists() || directory.join("token").exists() {
        return Err(Error::new(
            ErrorCode::AlreadyExists,
            "configuration or token already exists",
        ));
    }
    let mounts = directory.join("mounts");
    fs::create_dir_all(&mounts)?;
    let exchange = directory.join("exchange");
    fs::create_dir_all(&exchange)?;
    let token_file = directory.join("token");
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    create_file(&token_file, token.as_bytes())?;
    let config = Config {
        data_dir: directory.join("state"),
        listen,
        mount_roots: vec![mounts],
        directory_roots: vec![exchange],
        credentials: vec![TokenFile {
            principal: "owner".to_owned().try_into()?,
            token_file,
        }],
        allowed_origins: vec![],
        cache: CacheConfig::default(),
        validation: BTreeMap::new(),
        docker: docker(),
        remote: None,
    };
    create_file(&config_path, &serde_json::to_vec_pretty(&config)?)?;
    Ok(config_path)
}
