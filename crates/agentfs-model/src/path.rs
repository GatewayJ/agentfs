use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

/// An absolute path inside one workspace, never a host filesystem path.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(try_from = "String", into = "String")]
pub struct WorkspacePath(String);

impl WorkspacePath {
    pub fn root() -> Self {
        Self("/".to_owned())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn is_root(&self) -> bool {
        self.0 == "/"
    }

    pub fn lookup_key(&self) -> String {
        self.0.nfc().case_fold().nfc().collect()
    }

    pub fn parent(&self) -> Option<Self> {
        if self.is_root() {
            return None;
        }
        let offset = self.0.rfind('/').unwrap_or(0);
        Some(if offset == 0 {
            Self::root()
        } else {
            Self(self.0[..offset].to_owned())
        })
    }

    pub fn name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or("")
    }

    pub fn join(&self, name: &str) -> Result<Self> {
        validate_name(name)?;
        if self.is_root() {
            format!("/{name}").try_into()
        } else {
            format!("{}/{name}", self.0).try_into()
        }
    }

    pub fn contains(&self, other: &Self) -> bool {
        let key = self.lookup_key();
        let other = other.lookup_key();
        self.is_root() || other == key || other.starts_with(&format!("{key}/"))
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 255
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|c| c.is_control() || "/\\:*?\"<>|".contains(c))
    {
        return Err(Error::invalid("invalid or nonportable file name"));
    }
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(
                &stem[3..],
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            ));
    if reserved {
        return Err(Error::invalid("reserved Windows file name"));
    }
    Ok(())
}

impl TryFrom<String> for WorkspacePath {
    type Error = Error;
    fn try_from(value: String) -> Result<Self> {
        if value == "/" {
            return Ok(Self::root());
        }
        if !value.starts_with('/') || value.ends_with('/') || value.len() > 4096 {
            return Err(Error::invalid(
                "workspace path must be absolute and normalized",
            ));
        }
        for name in value[1..].split('/') {
            validate_name(name)?;
        }
        Ok(Self(value))
    }
}

impl From<WorkspacePath> for String {
    fn from(value: WorkspacePath) -> Self {
        value.0
    }
}

impl fmt::Display for WorkspacePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_portable_and_cannot_escape_workspace() {
        for path in [
            "relative",
            "/a/../b",
            "/a//b",
            "/a/",
            "/CON.txt",
            "/a\\b",
            "/trailing.",
            "/nul",
        ] {
            assert!(WorkspacePath::try_from(path.to_owned()).is_err(), "{path}");
        }
        let composed = WorkspacePath::try_from("/CAFÉ".to_owned()).unwrap();
        let decomposed = WorkspacePath::try_from("/cafe\u{301}".to_owned()).unwrap();
        assert_eq!(composed.lookup_key(), decomposed.lookup_key());
        assert!(
            !WorkspacePath::try_from("/a".to_owned())
                .unwrap()
                .contains(&WorkspacePath::try_from("/ab".to_owned()).unwrap())
        );
    }
}
