use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de::Error as _;
use std::borrow::Borrow;
use std::fmt;
use std::ops::Deref;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

/// Absolute, lexically normalized path used at the Windows sandbox boundary.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct AbsolutePathBuf(PathBuf);

impl AbsolutePathBuf {
    pub fn resolve_path_against_base<P: AsRef<Path>, B: AsRef<Path>>(
        path: P,
        base_path: B,
    ) -> Self {
        let path = path.as_ref();
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            base_path.as_ref().join(path)
        };
        Self(normalize_path(&joined))
    }

    pub fn from_absolute_path<P: AsRef<Path>>(path: P) -> std::io::Result<Self> {
        let path = path.as_ref();
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        Ok(Self(normalize_path(&absolute)))
    }

    pub fn from_absolute_path_checked<P: AsRef<Path>>(path: P) -> std::io::Result<Self> {
        let path = path.as_ref();
        if !path.is_absolute() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("path is not absolute: {}", path.display()),
            ));
        }
        Ok(Self(normalize_path(path)))
    }

    pub fn current_dir() -> std::io::Result<Self> {
        Self::from_absolute_path(std::env::current_dir()?)
    }

    pub fn join<P: AsRef<Path>>(&self, path: P) -> Self {
        Self::resolve_path_against_base(path, &self.0)
    }

    pub fn canonicalize(&self) -> std::io::Result<Self> {
        dunce::canonicalize(&self.0).map(Self)
    }

    pub fn parent(&self) -> Option<Self> {
        self.0.parent().map(|path| Self(path.to_path_buf()))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }

    pub fn to_path_buf(&self) -> PathBuf {
        self.0.clone()
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

impl<'de> Deserialize<'de> for AbsolutePathBuf {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let path = PathBuf::deserialize(deserializer)?;
        Self::from_absolute_path_checked(path).map_err(D::Error::custom)
    }
}

impl AsRef<Path> for AbsolutePathBuf {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

impl Borrow<Path> for AbsolutePathBuf {
    fn borrow(&self) -> &Path {
        self.as_path()
    }
}

impl Deref for AbsolutePathBuf {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.as_path()
    }
}

impl TryFrom<&Path> for AbsolutePathBuf {
    type Error = std::io::Error;

    fn try_from(value: &Path) -> Result<Self, Self::Error> {
        Self::from_absolute_path_checked(value)
    }
}

impl TryFrom<PathBuf> for AbsolutePathBuf {
    type Error = std::io::Error;

    fn try_from(value: PathBuf) -> Result<Self, Self::Error> {
        Self::from_absolute_path_checked(value)
    }
}

impl fmt::Display for AbsolutePathBuf {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.display().fmt(formatter)
    }
}
