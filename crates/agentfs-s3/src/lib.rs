//! S3 and RustFS storage with immutable content and conditional reference updates.
use agentfs_model::*;
use agentfs_ports::*;
use async_trait::async_trait;
use bytes::Bytes;
use futures::TryStreamExt;
use object_store::{
    ObjectStore, ObjectStoreExt, PutMode, UpdateVersion,
    aws::{AmazonS3, AmazonS3Builder, S3ConditionalPut},
    multipart::{MultipartStore, PartId},
    path::Path,
    signer::{HeaderName, HeaderValue, Method, SignedUrlOptions, Signer},
};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::{sync::Arc, time::Duration};
use tokio::io::AsyncReadExt;
use tokio_util::io::StreamReader;

const MIN_PART_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PART_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3Config {
    pub bucket: String,
    #[serde(default = "default_prefix")]
    pub prefix: String,
    #[serde(default = "default_region")]
    pub region: String,
    pub endpoint: Option<String>,
    #[serde(default)]
    pub allow_http: bool,
}
fn default_prefix() -> String {
    "agentfs/v1".into()
}
fn default_region() -> String {
    "us-east-1".into()
}

pub struct S3Backend {
    client: AmazonS3,
    http: reqwest::Client,
    prefix: String,
}
impl std::fmt::Debug for S3Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Backend")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
struct Version {
    etag: String,
    version: Option<String>,
}

impl S3Backend {
    pub fn new(config: S3Config) -> Result<Arc<Self>> {
        if config.bucket.is_empty() {
            return Err(Error::invalid("S3 bucket is required"));
        }
        let prefix = Path::parse(config.prefix.trim_matches('/'))
            .map_err(|error| Error::invalid(error.to_string()))?
            .to_string();
        if prefix.is_empty() {
            return Err(Error::invalid("S3 namespace prefix must not be empty"));
        }
        let mut builder = AmazonS3Builder::from_env()
            .with_bucket_name(config.bucket)
            .with_region(config.region)
            .with_allow_http(config.allow_http)
            .with_conditional_put(S3ConditionalPut::ETagMatch)
            .with_virtual_hosted_style_request(false);
        if let Some(endpoint) = config.endpoint {
            let url = object_store::signer::Url::parse(&endpoint)
                .map_err(|_| Error::invalid("invalid S3 endpoint"))?;
            if !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(Error::invalid(
                    "S3 endpoint must not contain credentials, a query or a fragment",
                ));
            }
            builder = builder.with_endpoint(endpoint);
        }
        let client = builder.build().map_err(store_error)?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(http_error)?;
        Ok(Arc::new(Self {
            client,
            http,
            prefix,
        }))
    }

    fn object_key(&self, workspace: WorkspaceId, object: &ObjectId) -> Path {
        Path::from(format!("{}/{workspace}/objects/{object}", self.prefix))
    }
    fn ref_key(&self, key: &RefKey) -> Path {
        Path::from(match key {
            RefKey::Workspace(workspace) => format!("{}/{workspace}/control.json", self.prefix),
            RefKey::Branch { workspace, branch } => {
                format!("{}/{workspace}/branches/{branch}.json", self.prefix)
            }
        })
    }

    async fn verify_remote(&self, workspace: WorkspaceId, object: &ObjectRef) -> Result<()> {
        let mut stream = self.download(workspace, object).await?;
        let mut digest = object_hasher(object.kind);
        let mut total = 0u64;
        let mut buffer = vec![0; IO_BUFFER_BYTES];
        loop {
            let size = stream.reader.read(&mut buffer).await?;
            if size == 0 {
                break;
            }
            total = total
                .checked_add(size as u64)
                .ok_or_else(|| Error::integrity("remote object size overflow"))?;
            if total > object.size {
                return Err(Error::integrity("remote object exceeded its declared size"));
            }
            digest.update(&buffer[..size]);
        }
        if total != object.size || finish_digest(digest) != object.id {
            return Err(Error::integrity(
                "existing remote object failed verification",
            ));
        }
        Ok(())
    }

    async fn complete_immutable(
        &self,
        path: &Path,
        upload: &str,
        parts: Vec<PartId>,
    ) -> Result<()> {
        let header = HeaderName::from_static("if-none-match");
        let options = SignedUrlOptions::new()
            .with_query([("uploadId", upload)])
            .with_signed_header(header.clone(), HeaderValue::from_static("*"));
        let url = self
            .client
            .signed_url_opts(Method::POST, path, Duration::from_secs(300), &options)
            .await
            .map_err(store_error)?;
        let mut body = String::from("<CompleteMultipartUpload>");
        for (index, part) in parts.into_iter().enumerate() {
            body.push_str(&format!(
                "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
                index + 1,
                quick_xml::escape::escape(&part.content_id)
            ));
        }
        body.push_str("</CompleteMultipartUpload>");
        let response = self
            .http
            .post(url)
            .header(header, "*")
            .header("content-type", "application/xml")
            .body(body)
            .send()
            .await
            .map_err(http_error)?;
        let status = response.status();
        if status.as_u16() == 412 {
            return Err(Error::new(
                ErrorCode::AlreadyExists,
                "remote object already exists",
            ));
        }
        if !status.is_success() {
            return Err(Error::new(
                ErrorCode::RemoteUnknown,
                format!("multipart completion returned HTTP {}", status.as_u16()),
            )
            .retryable());
        }
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(bytes) = stream.try_next().await.map_err(http_error)? {
            if body.len() + bytes.len() > MAX_METADATA_BYTES {
                return Err(Error::integrity(
                    "multipart completion response is too large",
                ));
            }
            body.extend_from_slice(&bytes);
        }
        let mut reader = quick_xml::Reader::from_reader(body.as_slice());
        loop {
            match reader.read_event() {
                Ok(quick_xml::events::Event::Start(tag)) => {
                    return if tag.local_name().as_ref() == b"CompleteMultipartUploadResult" {
                        Ok(())
                    } else {
                        Err(Error::new(
                            ErrorCode::RemoteUnknown,
                            "multipart completion returned an embedded error",
                        )
                        .retryable())
                    };
                }
                Ok(quick_xml::events::Event::Eof) | Err(_) => {
                    return Err(Error::new(
                        ErrorCode::RemoteUnknown,
                        "multipart completion response is incomplete",
                    )
                    .retryable());
                }
                _ => (),
            }
        }
    }

    async fn upload_multipart(
        &self,
        workspace: WorkspaceId,
        mut source: ObjectStream,
    ) -> Result<()> {
        let part_size = source
            .reference
            .size
            .div_ceil(10_000)
            .max(MIN_PART_BYTES)
            .next_multiple_of(1024 * 1024);
        if part_size > MAX_PART_BYTES {
            return Err(Error::new(
                ErrorCode::CapacityExceeded,
                "file exceeds the configured multipart memory boundary",
            ));
        }
        let path = self.object_key(workspace, &source.reference.id);
        let upload = self
            .client
            .create_multipart(&path)
            .await
            .map_err(store_error)?;
        let result = async {
            let mut parts = Vec::new();
            let mut remaining = source.reference.size;
            let mut digest = object_hasher(source.reference.kind);
            while remaining != 0 {
                let size = remaining.min(part_size) as usize;
                let mut buffer = vec![0; size];
                source.reader.read_exact(&mut buffer).await?;
                digest.update(&buffer);
                let part = self
                    .client
                    .put_part(&path, &upload, parts.len(), Bytes::from(buffer).into())
                    .await
                    .map_err(store_error)?;
                parts.push(part);
                remaining -= size as u64;
            }
            let mut extra = [0u8; 1];
            if source.reader.read(&mut extra).await? != 0
                || finish_digest(digest) != source.reference.id
            {
                return Err(Error::integrity("upload source failed digest verification"));
            }
            self.complete_immutable(&path, &upload, parts).await
        }
        .await;
        if result.is_err() {
            let _ = self.client.abort_multipart(&path, &upload).await;
            if matches!(
                result.as_ref().err().map(|error| error.code),
                Some(ErrorCode::AlreadyExists | ErrorCode::RemoteUnknown | ErrorCode::Unavailable)
            ) {
                match self.verify_remote(workspace, &source.reference).await {
                    Ok(()) => return Ok(()),
                    Err(error) if error.code == ErrorCode::Integrity => return Err(error),
                    Err(_) => (),
                }
            }
        }
        result
    }
}

fn store_error(error: object_store::Error) -> Error {
    let code = match error {
        object_store::Error::NotFound { .. } => ErrorCode::NotFound,
        object_store::Error::AlreadyExists { .. } => ErrorCode::AlreadyExists,
        object_store::Error::Precondition { .. } | object_store::Error::NotModified { .. } => {
            ErrorCode::Conflict
        }
        object_store::Error::PermissionDenied { .. }
        | object_store::Error::Unauthenticated { .. } => ErrorCode::PermissionDenied,
        object_store::Error::NotSupported { .. } | object_store::Error::NotImplemented { .. } => {
            ErrorCode::Unsupported
        }
        object_store::Error::InvalidPath { .. }
        | object_store::Error::UnknownConfigurationKey { .. } => ErrorCode::InvalidArgument,
        _ => ErrorCode::Unavailable,
    };
    let mut mapped = Error::new(code, error.to_string());
    mapped.retryable = code == ErrorCode::Unavailable;
    mapped
}
fn http_error(error: reqwest::Error) -> Error {
    Error::new(ErrorCode::RemoteUnknown, error.without_url().to_string()).retryable()
}

#[async_trait]
impl RemoteObjectStore for S3Backend {
    async fn upload(&self, workspace: WorkspaceId, mut source: ObjectStream) -> Result<()> {
        let path = self.object_key(workspace, &source.reference.id);
        match self.client.head(&path).await {
            Ok(_) => return self.verify_remote(workspace, &source.reference).await,
            Err(object_store::Error::NotFound { .. }) => (),
            Err(error) => return Err(store_error(error)),
        }
        if source.reference.size > MIN_PART_BYTES {
            return self.upload_multipart(workspace, source).await;
        }
        let mut bytes = Vec::with_capacity(source.reference.size as usize);
        (&mut source.reader)
            .take(source.reference.size + 1)
            .read_to_end(&mut bytes)
            .await?;
        verify_object(&source.reference, &bytes)?;
        match self
            .client
            .put_opts(&path, Bytes::from(bytes).into(), PutMode::Create.into())
            .await
        {
            Ok(_) => Ok(()),
            Err(object_store::Error::AlreadyExists { .. }) => {
                self.verify_remote(workspace, &source.reference).await
            }
            Err(error) => Err(store_error(error)),
        }
    }

    async fn download(&self, workspace: WorkspaceId, object: &ObjectRef) -> Result<ObjectStream> {
        let result = self
            .client
            .get(&self.object_key(workspace, &object.id))
            .await
            .map_err(store_error)?;
        if result.meta.size != object.size {
            return Err(Error::integrity(
                "remote object length does not match its reference",
            ));
        }
        let reader = StreamReader::new(result.into_stream().map_err(std::io::Error::from));
        Ok(ObjectStream {
            reference: object.clone(),
            reader: Box::pin(reader),
        })
    }

    async fn contains(&self, workspace: WorkspaceId, object: &ObjectRef) -> Result<bool> {
        match self.verify_remote(workspace, object).await {
            Ok(()) => Ok(true),
            Err(error) if error.code == ErrorCode::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

#[async_trait]
impl RemoteRefStore for S3Backend {
    async fn get(&self, key: &RefKey) -> Result<Option<VersionedRef>> {
        let result = match self.client.get(&self.ref_key(key)).await {
            Ok(result) => result,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(store_error(error)),
        };
        if result.meta.size > MAX_METADATA_BYTES as u64 {
            return Err(Error::integrity("remote reference exceeds format limit"));
        }
        let version = Version {
            etag: result
                .meta
                .e_tag
                .clone()
                .ok_or_else(|| Error::integrity("S3 response omitted ETag"))?,
            version: result.meta.version.clone(),
        };
        let size = result.meta.size;
        let mut reader =
            StreamReader::new(result.into_stream().map_err(std::io::Error::from)).take(size + 1);
        let mut bytes = Vec::with_capacity(size as usize);
        reader.read_to_end(&mut bytes).await?;
        if bytes.len() as u64 != size {
            return Err(Error::integrity("remote reference length mismatch"));
        }
        Ok(Some(VersionedRef {
            version: serde_json::to_string(&version)?,
            bytes: bytes.into(),
        }))
    }

    async fn compare_exchange(&self, update: RefUpdate) -> Result<CasOutcome> {
        if update.bytes.len() > MAX_METADATA_BYTES {
            return Err(Error::invalid("remote reference exceeds format limit"));
        }
        let mode = if let Some(version) = update.expected_version {
            let version: Version = serde_json::from_str(&version)?;
            PutMode::Update(UpdateVersion {
                e_tag: Some(version.etag),
                version: version.version,
            })
        } else {
            PutMode::Create
        };
        match self
            .client
            .put_opts(&self.ref_key(&update.key), update.bytes.into(), mode.into())
            .await
        {
            Ok(result) => Ok(CasOutcome::Applied {
                version: serde_json::to_string(&Version {
                    etag: result
                        .e_tag
                        .ok_or_else(|| Error::integrity("S3 write response omitted ETag"))?,
                    version: result.version,
                })?,
            }),
            Err(
                object_store::Error::AlreadyExists { .. }
                | object_store::Error::Precondition { .. }
                | object_store::Error::NotModified { .. },
            ) => Ok(CasOutcome::Conflict),
            Err(error) => {
                let error = store_error(error);
                if error.retryable {
                    Ok(CasOutcome::Unknown)
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn list_branches(&self, workspace: WorkspaceId) -> Result<Vec<BranchId>> {
        let prefix = Path::from(format!("{}/{workspace}/branches/", self.prefix));
        let mut stream = self.client.list(Some(&prefix));
        let mut branches = Vec::new();
        while let Some(object) = stream.try_next().await.map_err(store_error)? {
            if let Some(name) = object
                .location
                .filename()
                .and_then(|name| name.strip_suffix(".json"))
            {
                branches.push(name.parse()?);
            }
        }
        branches.sort();
        branches.dedup();
        Ok(branches)
    }
}
