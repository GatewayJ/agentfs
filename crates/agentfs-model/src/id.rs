use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};
use uuid::Uuid;

macro_rules! identity {
    ($($name:ident),+ $(,)?) => {$ (
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            pub fn new() -> Self { Self(Uuid::new_v4()) }
        }
        impl Default for $name {
            fn default() -> Self { Self::new() }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) }
        }
        impl FromStr for $name {
            type Err = Error;
            fn from_str(s: &str) -> Result<Self> {
                Uuid::parse_str(s).map(Self).map_err(|_| Error::invalid(concat!("invalid ", stringify!($name))))
            }
        }
    )+ };
}

identity!(
    WorkspaceId,
    LocationId,
    BranchId,
    RevisionId,
    OperationId,
    MountId,
    MergeId,
    HandleId
);

#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct InodeId(pub u64);

impl InodeId {
    pub const ROOT: Self = Self(1);
}

#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(try_from = "String", into = "String")]
pub struct ObjectId(String);

impl ObjectId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ObjectId {
    type Error = Error;
    fn try_from(value: String) -> Result<Self> {
        if value.len() != 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::invalid(
                "object identity must be 64 lowercase hexadecimal characters",
            ));
        }
        Ok(Self(value))
    }
}

impl From<ObjectId> for String {
    fn from(value: ObjectId) -> Self {
        value.0
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ObjectId {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        s.to_owned().try_into()
    }
}
