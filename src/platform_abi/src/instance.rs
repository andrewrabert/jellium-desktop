use std::fmt;
use std::io::{self, ErrorKind};
use std::ops::Deref;
use std::path::Path;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};
use uuid::Uuid;

#[derive(Clone, Copy, Debug)]
pub struct InstanceId(Uuid);

impl InstanceId {
    fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Deref for InstanceId {
    type Target = Uuid;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.simple())
    }
}

impl Serialize for InstanceId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0.simple())
    }
}

impl<'de> Deserialize<'de> for InstanceId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Uuid::try_parse(&raw).map(Self).map_err(de::Error::custom)
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct Instance {
    id: InstanceId,
}

const FILE_NAME: &str = "instance.json";

enum Read {
    Parsed(Instance),
    Missing,
    Invalid(serde_json::Error),
}

impl Instance {
    pub fn for_config_dir(config_dir: &Path) -> io::Result<Self> {
        let path = config_dir.join(FILE_NAME);
        let instance = match Self::read(&path)? {
            Read::Parsed(instance) => return Ok(instance),
            Read::Missing => Self::mint(),
            Read::Invalid(e) => {
                tracing::warn!("overwriting invalid instance file {}: {e}", path.display());
                Self::mint()
            }
        };
        instance.save(&path)?;
        Ok(instance)
    }

    #[must_use]
    pub fn id(&self) -> InstanceId {
        self.id
    }

    fn mint() -> Self {
        Self {
            id: InstanceId::new(),
        }
    }

    fn read(path: &Path) -> io::Result<Read> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Read::Missing),
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("read {}: {e}", path.display()),
                ));
            }
        };
        Ok(match serde_json::from_slice(&bytes) {
            Ok(instance) => Read::Parsed(instance),
            Err(e) => Read::Invalid(e),
        })
    }

    fn save(&self, path: &Path) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
        jfn_paths::write_atomic(path, &bytes)
            .map_err(|e| io::Error::new(e.kind(), format!("write {}: {e}", path.display())))
    }
}
