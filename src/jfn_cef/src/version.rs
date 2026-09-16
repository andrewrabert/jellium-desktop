use std::ffi::CStr;
use std::fmt;
use std::os::raw::c_int;

use serde::{Serialize, Serializer};

unsafe extern "C" {
    fn cef_version_info(entry: c_int) -> c_int;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShortHash([u8; 7]);

impl ShortHash {
    pub fn from_prefix(full: &str) -> Option<Self> {
        let bytes = full.as_bytes().get(..7)?;
        if !bytes.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        let mut hash = [0u8; 7];
        hash.copy_from_slice(bytes);
        Some(Self(hash))
    }
}

impl fmt::Display for ShortHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use std::fmt::Write;
        for byte in self.0 {
            f.write_char(char::from(byte))?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CefVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub commit: ShortHash,
    pub chromium: [u32; 4],
}

impl fmt::Display for CefVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = self;
        write!(
            f,
            "{}.{}.{}+g{}+chromium-{}.{}.{}.{}",
            v.major,
            v.minor,
            v.patch,
            v.commit,
            v.chromium[0],
            v.chromium[1],
            v.chromium[2],
            v.chromium[3],
        )
    }
}

impl Serialize for CefVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

fn commit_hash(_runtime: &crate::runtime::LibraryLoaded) -> Result<ShortHash, VersionError> {
    let ptr = unsafe { cef::sys::cef_api_hash(cef::sys::CEF_API_VERSION_LAST, 2) };
    if ptr.is_null() {
        return Err(VersionError::MissingCommitHash);
    }
    let full = unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| VersionError::InvalidCommitHash)?;
    ShortHash::from_prefix(full).ok_or(VersionError::InvalidCommitHash)
}

pub(crate) fn probe(runtime: &crate::runtime::LibraryLoaded) -> Result<CefVersion, VersionError> {
    let commit = commit_hash(runtime)?;
    let v = |entry| {
        let value = unsafe { cef_version_info(entry) };
        u32::try_from(value).map_err(|_| VersionError::InvalidComponent { entry, value })
    };
    Ok(CefVersion {
        major: v(0)?,
        minor: v(1)?,
        patch: v(2)?,
        commit,
        chromium: [v(4)?, v(5)?, v(6)?, v(7)?],
    })
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VersionError {
    #[error("CEF version component {entry} is invalid: {value}")]
    InvalidComponent { entry: c_int, value: c_int },
    #[error("CEF returned no commit hash")]
    MissingCommitHash,
    #[error("CEF returned an invalid commit hash")]
    InvalidCommitHash,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn prefix_contract_and_exact_version_format() {
        for invalid in ["", "123456", "abcdefz", "ébcdef0"] {
            assert!(ShortHash::from_prefix(invalid).is_none());
        }
        assert_eq!(
            ShortHash::from_prefix("ABCDEF0ignored")
                .unwrap()
                .to_string(),
            "ABCDEF0"
        );
        let version = CefVersion {
            major: 151,
            minor: 3,
            patch: 16,
            commit: ShortHash::from_prefix("abcdef0").unwrap(),
            chromium: [151, 0, 7871, 2],
        };
        assert_eq!(
            version.to_string(),
            "151.3.16+gabcdef0+chromium-151.0.7871.2"
        );
        assert_eq!(
            serde_json::to_string(&version).unwrap(),
            "\"151.3.16+gabcdef0+chromium-151.0.7871.2\""
        );
    }
}
