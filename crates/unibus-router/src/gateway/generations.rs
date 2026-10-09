//! Owner-local durable generation floors. No ACLs, credentials or discovery data.
use super::GenerationStore;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::PathBuf,
};
use unibus::gateway::{ResourceId, identifier};

const MAX_BYTES: usize = 65536;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    schema_version: u32,
    owner: String,
    entries: Vec<Entry>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    resource: ResourceId,
    generation: u64,
}
pub struct GenerationFile {
    path: PathBuf,
    // OS lock survives file replacement and is released even after process death.
    _lock: File,
}
impl GenerationFile {
    pub fn open(path: PathBuf) -> Result<Self> {
        ensure!(
            path.is_absolute() && path.file_name().is_some(),
            "generation state needs an absolute file path"
        );
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("state has no parent"))?;
        let metadata = std::fs::symlink_metadata(parent)?;
        ensure!(metadata.is_dir(), "state parent must be a real directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                metadata.permissions().mode() & 0o077 == 0,
                "state directory must be owner-private"
            );
        }
        let mut lock_path = path.as_os_str().to_owned();
        lock_path.push(".lock");
        let lock_path = PathBuf::from(lock_path);
        match std::fs::symlink_metadata(&lock_path) {
            Ok(_) => validate_file(&lock_path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(lock_path)?;
        lock.try_lock().map_err(|error| {
            anyhow::anyhow!("generation state already owned or lock failed: {error}")
        })?;
        Ok(Self { path, _lock: lock })
    }
}
fn validate_file(path: &std::path::Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file(),
        "generation state must be a regular file, not a symlink"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "generation state must be owner-private"
        );
    }
    Ok(())
}
impl GenerationStore for GenerationFile {
    fn load(&self, owner: &str) -> Result<BTreeMap<ResourceId, u64>> {
        identifier(owner)?;
        // A missing initial state is allowed; malformed existing state is not.
        match std::fs::symlink_metadata(&self.path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BTreeMap::new());
            }
            Err(error) => return Err(error.into()),
            Ok(_) => validate_file(&self.path)?,
        }
        let mut bytes = Vec::new();
        File::open(&self.path)?
            .take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= MAX_BYTES,
            "generation state exceeds byte bound"
        );
        let document: Document = serde_json::from_slice(&bytes)?;
        ensure!(
            document.schema_version == 1
                && document.owner == owner
                && document.entries.len() <= 128,
            "invalid generation state schema, owner or count"
        );
        let mut floors = BTreeMap::new();
        for entry in document.entries {
            entry.resource.validate()?;
            ensure!(
                entry.generation > 0 && floors.insert(entry.resource, entry.generation).is_none(),
                "invalid or duplicate generation floor"
            );
        }
        Ok(floors)
    }
    fn persist(&self, owner: &str, floors: &BTreeMap<ResourceId, u64>) -> Result<()> {
        identifier(owner)?;
        ensure!(floors.len() <= 128, "generation floors exceed bound");
        let entries = floors
            .iter()
            .map(|(resource, generation)| {
                resource.validate()?;
                ensure!(*generation > 0, "invalid generation floor");
                Ok(Entry {
                    resource: resource.clone(),
                    generation: *generation,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let bytes = serde_json::to_vec(&Document {
            schema_version: 1,
            owner: owner.into(),
            entries,
        })?;
        ensure!(
            bytes.len() <= MAX_BYTES,
            "generation state exceeds byte bound"
        );
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("state has no parent"))?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary.persist(&self.path).map_err(|error| error.error)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_locked_state_round_trips_and_checks_owner() {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let path = directory.path().join("generations.json");
        let store = GenerationFile::open(path.clone()).unwrap();
        assert!(GenerationFile::open(path.clone()).is_err());
        let resource = serde_json::from_value(
            serde_json::json!({"scope":{"kind":"peer","id":"host"},"key":"canvas"}),
        )
        .unwrap();
        let floors = BTreeMap::from([(resource, 2)]);
        store.persist("host", &floors).unwrap();
        assert_eq!(store.load("host").unwrap(), floors);
        assert!(store.load("other").is_err());
        std::fs::write(&path, b"not-json").unwrap();
        assert!(store.load("host").is_err());
        std::fs::write(&path, vec![b' '; MAX_BYTES + 1]).unwrap();
        assert!(store.load("host").is_err());
        store.persist("host", &floors).unwrap();
        drop(store);
        assert_eq!(
            GenerationFile::open(path).unwrap().load("host").unwrap(),
            floors
        );
    }
    #[cfg(unix)]
    #[test]
    fn unsafe_permissions_and_lock_symlinks_fail_closed() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = directory.path().join("generations.json");
        assert!(GenerationFile::open(path.clone()).is_err());
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        symlink(
            directory.path().join("missing"),
            directory.path().join("generations.json.lock"),
        )
        .unwrap();
        assert!(GenerationFile::open(path).is_err());
    }
}
