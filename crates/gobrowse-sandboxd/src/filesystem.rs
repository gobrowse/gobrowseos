use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use gobrowse_core::sandbox::{
    FilesystemEntry, FilesystemEntryKind, MAX_DIRECTORY_ENTRIES, MAX_FILE_PAYLOAD_BYTES,
    validate_workspace_path,
};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum FilesystemError {
    #[error("invalid workspace path")]
    InvalidPath,
    #[error("workspace does not exist")]
    WorkspaceNotFound,
    #[error("filesystem entry does not exist")]
    NotFound,
    #[error("symlinks and special files are not permitted")]
    UnsafeEntry,
    #[error("filesystem operation exceeds its configured bound")]
    LimitExceeded,
    #[error("filesystem entry already exists")]
    Conflict,
    #[error("filesystem operation failed")]
    Io(#[source] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct Filesystem {
    root: PathBuf,
}

impl Filesystem {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, FilesystemError> {
        fs::create_dir_all(root.as_ref()).map_err(FilesystemError::Io)?;
        let metadata = fs::symlink_metadata(root.as_ref()).map_err(FilesystemError::Io)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(FilesystemError::UnsafeEntry);
        }
        let root = fs::canonicalize(root.as_ref()).map_err(FilesystemError::Io)?;
        Ok(Self { root })
    }

    pub fn workspace_path(&self, workspace_id: Uuid) -> PathBuf {
        self.root.join(workspace_id.to_string())
    }

    #[cfg(test)]
    pub(crate) fn ensure_workspace(&self, workspace_id: Uuid) -> Result<PathBuf, FilesystemError> {
        let path = self.workspace_path(workspace_id);
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&path).map_err(FilesystemError::Io)?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(FilesystemError::UnsafeEntry);
                }
            }
            Err(error) => return Err(FilesystemError::Io(error)),
        }
        Ok(path)
    }

    pub fn require_directory(
        &self,
        workspace_id: Uuid,
        path: &str,
    ) -> Result<PathBuf, FilesystemError> {
        let resolved = self.resolve_existing(workspace_id, path)?;
        if !fs::symlink_metadata(&resolved)
            .map_err(map_not_found)?
            .is_dir()
        {
            return Err(FilesystemError::UnsafeEntry);
        }
        Ok(resolved)
    }

    pub fn list(
        &self,
        workspace_id: Uuid,
        path: &str,
    ) -> Result<Vec<FilesystemEntry>, FilesystemError> {
        let directory = self.require_directory(workspace_id, path)?;
        let mut entries = Vec::new();
        for entry in fs::read_dir(directory).map_err(map_not_found)? {
            if entries.len() == MAX_DIRECTORY_ENTRIES {
                return Err(FilesystemError::LimitExceeded);
            }
            let entry = entry.map_err(FilesystemError::Io)?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| FilesystemError::UnsafeEntry)?;
            let metadata = fs::symlink_metadata(entry.path()).map_err(FilesystemError::Io)?;
            let kind = if metadata.file_type().is_file() {
                FilesystemEntryKind::File
            } else if metadata.file_type().is_dir() {
                FilesystemEntryKind::Directory
            } else {
                return Err(FilesystemError::UnsafeEntry);
            };
            entries.push(FilesystemEntry {
                name,
                kind,
                size: metadata.len(),
            });
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(entries)
    }

    pub fn read(&self, workspace_id: Uuid, path: &str) -> Result<Vec<u8>, FilesystemError> {
        let path = self.resolve_existing(workspace_id, path)?;
        let metadata = fs::symlink_metadata(&path).map_err(map_not_found)?;
        if !metadata.file_type().is_file() {
            return Err(FilesystemError::UnsafeEntry);
        }
        if metadata.len() > MAX_FILE_PAYLOAD_BYTES as u64 {
            return Err(FilesystemError::LimitExceeded);
        }

        // std has no portable no-follow open. Every component is checked immediately before
        // open and daemon requests are serialized, but a sandbox process can still race this
        // check. Deployments should additionally confine sandboxd with mount/LSM policy.
        let mut file = File::open(path).map_err(map_not_found)?;
        let opened_metadata = file.metadata().map_err(FilesystemError::Io)?;
        if !opened_metadata.file_type().is_file()
            || opened_metadata.dev() != metadata.dev()
            || opened_metadata.ino() != metadata.ino()
        {
            return Err(FilesystemError::UnsafeEntry);
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        Read::by_ref(&mut file)
            .take(MAX_FILE_PAYLOAD_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(FilesystemError::Io)?;
        if bytes.len() > MAX_FILE_PAYLOAD_BYTES {
            return Err(FilesystemError::LimitExceeded);
        }
        Ok(bytes)
    }

    pub fn write(
        &self,
        workspace_id: Uuid,
        path: &str,
        bytes: &[u8],
    ) -> Result<(), FilesystemError> {
        if bytes.len() > MAX_FILE_PAYLOAD_BYTES {
            return Err(FilesystemError::LimitExceeded);
        }
        let target = self.resolve_for_creation(workspace_id, path)?;
        if let Ok(metadata) = fs::symlink_metadata(&target)
            && !metadata.file_type().is_file()
        {
            return Err(FilesystemError::UnsafeEntry);
        }
        let parent = target.parent().ok_or(FilesystemError::InvalidPath)?;
        let temporary = parent.join(format!(".gobrowse-write-{}", Uuid::new_v4()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(FilesystemError::Io)?;
            file.write_all(bytes).map_err(FilesystemError::Io)?;
            file.sync_all().map_err(FilesystemError::Io)?;
            fs::rename(&temporary, &target).map_err(FilesystemError::Io)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub fn mkdir(&self, workspace_id: Uuid, path: &str) -> Result<(), FilesystemError> {
        let path = self.resolve_for_creation(workspace_id, path)?;
        match fs::create_dir(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(FilesystemError::Conflict)
            }
            Err(error) => Err(FilesystemError::Io(error)),
        }
    }

    pub fn rename(&self, workspace_id: Uuid, from: &str, to: &str) -> Result<(), FilesystemError> {
        let source = self.resolve_existing(workspace_id, from)?;
        let destination = self.resolve_for_creation(workspace_id, to)?;
        let source_metadata = fs::symlink_metadata(&source).map_err(map_not_found)?;
        if !source_metadata.file_type().is_file() && !source_metadata.file_type().is_dir() {
            return Err(FilesystemError::UnsafeEntry);
        }
        if fs::symlink_metadata(&destination).is_ok() {
            return Err(FilesystemError::Conflict);
        }
        fs::rename(source, destination).map_err(FilesystemError::Io)
    }

    pub fn delete(&self, workspace_id: Uuid, path: &str) -> Result<(), FilesystemError> {
        let workspace = self.workspace_root(workspace_id)?;
        let target = self.resolve_existing(workspace_id, path)?;
        if target == workspace {
            return Err(FilesystemError::InvalidPath);
        }
        let metadata = fs::symlink_metadata(&target).map_err(map_not_found)?;
        if metadata.file_type().is_file() {
            fs::remove_file(target).map_err(FilesystemError::Io)
        } else if metadata.file_type().is_dir() {
            fs::remove_dir(target).map_err(FilesystemError::Io)
        } else {
            Err(FilesystemError::UnsafeEntry)
        }
    }

    fn workspace_root(&self, workspace_id: Uuid) -> Result<PathBuf, FilesystemError> {
        let workspace = self.workspace_path(workspace_id);
        let metadata = fs::symlink_metadata(&workspace).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                FilesystemError::WorkspaceNotFound
            } else {
                FilesystemError::Io(error)
            }
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(FilesystemError::UnsafeEntry);
        }
        Ok(workspace)
    }

    fn resolve_existing(
        &self,
        workspace_id: Uuid,
        value: &str,
    ) -> Result<PathBuf, FilesystemError> {
        // Component checks and later path-based mutation cannot be made atomic without openat
        // primitives unavailable in this dependency set. The daemon serializes its own calls,
        // rejects every observed symlink/special file, and uses atomic writes, but a concurrent
        // sandbox process can still race parent replacement. Keep OS-level sandboxd confinement.
        let relative = validate_workspace_path(value).map_err(|_| FilesystemError::InvalidPath)?;
        let workspace = self.workspace_root(workspace_id)?;
        let target = workspace.join(&relative);
        let mut current = workspace;
        for component in relative.components() {
            current.push(component.as_os_str());
            let metadata = fs::symlink_metadata(&current).map_err(map_not_found)?;
            if metadata.file_type().is_symlink() {
                return Err(FilesystemError::UnsafeEntry);
            }
            if current != target && !metadata.is_dir() {
                return Err(FilesystemError::UnsafeEntry);
            }
        }
        Ok(current)
    }

    fn resolve_for_creation(
        &self,
        workspace_id: Uuid,
        value: &str,
    ) -> Result<PathBuf, FilesystemError> {
        let relative = validate_workspace_path(value).map_err(|_| FilesystemError::InvalidPath)?;
        if relative == Path::new(".") {
            return Err(FilesystemError::InvalidPath);
        }
        let workspace = self.workspace_root(workspace_id)?;
        let parent = relative
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        self.resolve_existing(
            workspace_id,
            parent.to_str().ok_or(FilesystemError::InvalidPath)?,
        )?;
        Ok(workspace.join(relative))
    }
}

fn map_not_found(error: std::io::Error) -> FilesystemError {
    if error.kind() == std::io::ErrorKind::NotFound {
        FilesystemError::NotFound
    } else {
        FilesystemError::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("gobrowse-sandboxd-{}", Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn filesystem_is_bounded_and_workspace_scoped() {
        let root = TestDirectory::new();
        let filesystem = Filesystem::new(&root.0).unwrap();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        filesystem.ensure_workspace(first).unwrap();
        filesystem.ensure_workspace(second).unwrap();
        filesystem.mkdir(first, "src").unwrap();
        filesystem.write(first, "src/main.rs", b"first").unwrap();

        assert_eq!(filesystem.read(first, "src/main.rs").unwrap(), b"first");
        assert!(matches!(
            filesystem.read(second, "src/main.rs"),
            Err(FilesystemError::NotFound)
        ));
        assert!(matches!(
            filesystem.read(first, "../second/secret"),
            Err(FilesystemError::InvalidPath)
        ));
        assert!(matches!(
            filesystem.write(first, "large", &vec![0; MAX_FILE_PAYLOAD_BYTES + 1]),
            Err(FilesystemError::LimitExceeded)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn filesystem_rejects_symlinks_and_special_files() {
        use std::os::unix::{fs::symlink, net::UnixListener};

        let root = TestDirectory::new();
        let filesystem = Filesystem::new(&root.0).unwrap();
        let workspace = Uuid::new_v4();
        let workspace_path = filesystem.ensure_workspace(workspace).unwrap();
        symlink("/etc/passwd", workspace_path.join("link")).unwrap();
        let _listener = UnixListener::bind(workspace_path.join("socket")).unwrap();

        assert!(matches!(
            filesystem.read(workspace, "link"),
            Err(FilesystemError::UnsafeEntry)
        ));
        assert!(matches!(
            filesystem.list(workspace, "."),
            Err(FilesystemError::UnsafeEntry)
        ));
        assert!(matches!(
            filesystem.delete(workspace, "socket"),
            Err(FilesystemError::UnsafeEntry)
        ));
        assert!(matches!(
            filesystem.rename(workspace, "socket", "moved"),
            Err(FilesystemError::UnsafeEntry)
        ));
    }

    #[test]
    fn rename_and_delete_cannot_cross_workspace_roots() {
        let root = TestDirectory::new();
        let filesystem = Filesystem::new(&root.0).unwrap();
        let workspace = Uuid::new_v4();
        filesystem.ensure_workspace(workspace).unwrap();
        filesystem.write(workspace, "source", b"data").unwrap();
        assert!(
            filesystem
                .rename(workspace, "source", "../escaped")
                .is_err()
        );
        filesystem.rename(workspace, "source", "target").unwrap();
        filesystem.delete(workspace, "target").unwrap();
        assert!(matches!(
            filesystem.delete(workspace, "."),
            Err(FilesystemError::InvalidPath)
        ));
    }
}
