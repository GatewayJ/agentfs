use crate::{Error, FORMAT_VERSION, InodeId, ObjectId, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    File,
    DirectoryIndex,
    InodeIndex,
    Tree,
    RecordIndex,
}

impl ObjectKind {
    pub fn tag(self) -> &'static [u8] {
        match self {
            Self::File => b"file",
            Self::DirectoryIndex => b"directory-index",
            Self::InodeIndex => b"inode-index",
            Self::Tree => b"tree",
            Self::RecordIndex => b"record-index",
        }
    }
}

#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct ObjectRef {
    pub id: ObjectId,
    pub kind: ObjectKind,
    pub size: u64,
}

pub fn object_hasher(kind: ObjectKind) -> Sha256 {
    let mut digest = Sha256::new();
    digest.update(b"agentfs\0");
    digest.update(FORMAT_VERSION.to_le_bytes());
    digest.update(kind.tag());
    digest.update([0]);
    digest
}

pub fn finish_digest(digest: Sha256) -> ObjectId {
    hex::encode(digest.finalize())
        .try_into()
        .expect("SHA-256 produces 64 hexadecimal characters")
}

pub fn object_ref(kind: ObjectKind, bytes: &[u8]) -> ObjectRef {
    let mut digest = object_hasher(kind);
    digest.update(bytes);
    ObjectRef {
        id: finish_digest(digest),
        kind,
        size: bytes.len() as u64,
    }
}

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(value)?)
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    Ok(serde_json::from_slice(bytes)?)
}

pub fn verify_object(reference: &ObjectRef, bytes: &[u8]) -> Result<()> {
    if object_ref(reference.kind, bytes) != *reference {
        return Err(Error::integrity(
            "object length or digest differs from its reference",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IndexEntry<T> {
    pub key: String,
    pub value: T,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IndexChild {
    pub first_key: String,
    pub object: ObjectRef,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IndexPage<T> {
    Leaf { entries: Vec<IndexEntry<T>> },
    Branch { children: Vec<IndexChild> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DirectoryEntry {
    pub parent: InodeId,
    pub name: String,
    pub inode: InodeId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TreeRoot {
    pub format_version: u32,
    pub directories: ObjectRef,
    pub inodes: ObjectRef,
}
