use std::{
    ffi::{CStr, OsStr, OsString},
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    path::{Path, PathBuf},
    sync::Arc,
};

use gobrowse_core::sandbox::{
    FilesystemEntry, FilesystemEntryKind, FilesystemMetadata, FilesystemSearchMatch,
    MAX_DIRECTORY_ENTRIES, MAX_FILE_PAYLOAD_BYTES, MAX_FILESYSTEM_SCANNED_ENTRIES,
    MAX_FILESYSTEM_SEARCH_ENCODED_BYTES, MAX_FILESYSTEM_SEARCH_QUERY_BYTES,
    MAX_FILESYSTEM_SEARCH_RESULTS, MAX_WORKSPACE_PATH_BYTES, MAX_WORKSPACE_PATH_COMPONENTS,
    WorkspaceStorageIdentity, validate_workspace_path, workspace_volume_name,
};
use rustix::{
    fs::{
        AtFlags, Dir, FileType, Mode, OFlags, RenameFlags, ResolveFlags, fchmod, fstat, fsync,
        mkdirat, open, openat2, renameat, renameat_with, unlinkat,
    },
    io::Errno,
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

const RESOLVE: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_MAGICLINKS)
    .union(ResolveFlags::NO_XDEV);
const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const METADATA_FLAGS: OFlags = OFlags::PATH.union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);
const READ_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NONBLOCK)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const CREATE_FLAGS: OFlags = OFlags::WRONLY
    .union(OFlags::CREATE)
    .union(OFlags::EXCL)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const STAGING_DIRECTORY: &str = ".gobrowse-staging";
const RECOVERY_DIRECTORY: &str = ".gobrowse-recovery";
const VOLUME_DATA_DIRECTORY: &str = "_data";
const MAX_RECOVERY_RECORD_BYTES: usize = 256 * 1024;
const MAX_INTERNAL_COPY_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_INTERNAL_COPY_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_INTERNAL_COPY_ENTRIES: usize = 16_384;

#[derive(Debug, Error)]
pub enum FilesystemError {
    #[error("invalid workspace path")]
    InvalidPath,
    #[error("workspace does not exist")]
    WorkspaceNotFound,
    #[error("filesystem entry does not exist")]
    NotFound,
    #[error("symlinks, hardlinks, mount crossings, and special files are not permitted")]
    UnsafeEntry,
    #[error("an unsafe published entry could not be contained")]
    ContainmentFailure,
    #[error("filesystem operation exceeds its configured bound")]
    LimitExceeded,
    #[error("filesystem entry already exists or changed concurrently")]
    Conflict,
    #[error("filesystem operation failed")]
    Io(#[source] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct Filesystem {
    root_path: PathBuf,
    root_fd: Arc<OwnedFd>,
    staging_fd: Arc<OwnedFd>,
    recovery: RecoveryStore,
}

#[derive(Debug, Clone)]
pub struct WorkspaceResolver {
    root_path: PathBuf,
    root_fd: Arc<OwnedFd>,
}

#[derive(Debug, Clone)]
pub struct RecoveryStore {
    directory: Arc<OwnedFd>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryRecord {
    pub workspace_id: Uuid,
    pub operation_id: Uuid,
    pub containers: Vec<String>,
}

impl Filesystem {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, FilesystemError> {
        if !root.as_ref().is_absolute() {
            return Err(FilesystemError::InvalidPath);
        }
        let root_fd = open(root.as_ref(), DIRECTORY_FLAGS, Mode::empty()).map_err(errno_io)?;
        ensure_directory(&root_fd)?;
        let root_stat = fstat(&root_fd).map_err(map_errno)?;
        if root_stat.st_mode & 0o022 != 0 {
            return Err(FilesystemError::UnsafeEntry);
        }

        // Probe openat2 with every required resolver flag. ENOSYS/EINVAL is fatal: there is no
        // pathname-based compatibility mode for kernels that cannot enforce this boundary.
        openat2(
            &root_fd,
            ".",
            METADATA_FLAGS | OFlags::DIRECTORY,
            Mode::empty(),
            RESOLVE,
        )
        .map_err(errno_io)?;

        match mkdirat(&root_fd, STAGING_DIRECTORY, Mode::RWXU) {
            Ok(()) => fsync(&root_fd).map_err(errno_io)?,
            Err(Errno::EXIST) => {}
            Err(error) => return Err(map_errno(error)),
        }
        let staging_fd = openat2(
            &root_fd,
            STAGING_DIRECTORY,
            DIRECTORY_FLAGS,
            Mode::empty(),
            RESOLVE,
        )
        .map_err(map_errno)?;
        let staging_stat = fstat(&staging_fd).map_err(map_errno)?;
        if FileType::from_raw_mode(staging_stat.st_mode) != FileType::Directory
            || staging_stat.st_dev != root_stat.st_dev
            || staging_stat.st_uid != root_stat.st_uid
            || staging_stat.st_mode & 0o077 != 0
        {
            return Err(FilesystemError::UnsafeEntry);
        }
        clear_directory(&staging_fd)?;

        match mkdirat(&root_fd, RECOVERY_DIRECTORY, Mode::RWXU) {
            Ok(()) => fsync(&root_fd).map_err(errno_io)?,
            Err(Errno::EXIST) => {}
            Err(error) => return Err(map_errno(error)),
        }
        let recovery_fd = openat2(
            &root_fd,
            RECOVERY_DIRECTORY,
            DIRECTORY_FLAGS,
            Mode::empty(),
            RESOLVE,
        )
        .map_err(map_errno)?;
        let recovery_stat = fstat(&recovery_fd).map_err(map_errno)?;
        if FileType::from_raw_mode(recovery_stat.st_mode) != FileType::Directory
            || recovery_stat.st_dev != root_stat.st_dev
            || recovery_stat.st_uid != root_stat.st_uid
            || recovery_stat.st_mode & 0o077 != 0
        {
            return Err(FilesystemError::UnsafeEntry);
        }

        Ok(Self {
            root_path: root.as_ref().to_path_buf(),
            root_fd: Arc::new(root_fd),
            staging_fd: Arc::new(staging_fd),
            recovery: RecoveryStore {
                directory: Arc::new(recovery_fd),
            },
        })
    }

    pub(crate) fn ensure_workspace(&self, workspace_id: Uuid) -> Result<PathBuf, FilesystemError> {
        let name = workspace_volume_name(workspace_id);
        match mkdirat(self.root_fd.as_fd(), &name, Mode::RWXU) {
            Ok(()) => fsync(self.root_fd.as_fd()).map_err(errno_io)?,
            Err(Errno::EXIST) => {}
            Err(error) => return Err(map_errno(error)),
        }
        let volume = openat2(
            self.root_fd.as_fd(),
            &name,
            DIRECTORY_FLAGS,
            Mode::empty(),
            RESOLVE,
        )
        .map_err(map_errno)?;
        match mkdirat(&volume, VOLUME_DATA_DIRECTORY, Mode::RWXU) {
            Ok(()) => fsync(&volume).map_err(errno_io)?,
            Err(Errno::EXIST) => {}
            Err(error) => return Err(map_errno(error)),
        }
        self.workspace(workspace_id)?;
        Ok(self.root_path.join(name).join(VOLUME_DATA_DIRECTORY))
    }

    pub fn resolver(&self) -> WorkspaceResolver {
        WorkspaceResolver {
            root_path: self.root_path.clone(),
            root_fd: Arc::clone(&self.root_fd),
        }
    }

    pub fn recovery_store(&self) -> RecoveryStore {
        self.recovery.clone()
    }

    pub fn require_directory(&self, workspace_id: Uuid, path: &str) -> Result<(), FilesystemError> {
        let workspace = self.workspace(workspace_id)?;
        let path = checked_path(path)?;
        let directory = open_beneath(&workspace, &path, DIRECTORY_FLAGS)?;
        ensure_directory(&directory)?;
        Ok(())
    }

    pub fn workspace_storage(
        &self,
        workspace_id: Uuid,
    ) -> Result<WorkspaceStorageIdentity, FilesystemError> {
        let workspace = self.workspace(workspace_id)?;
        let stat = fstat(&workspace).map_err(map_errno)?;
        ensure_directory(&workspace)?;
        Ok(WorkspaceStorageIdentity {
            volume_name: workspace_volume_name(workspace_id),
            device: stat.st_dev,
            inode: stat.st_ino,
        })
    }

    pub fn list(
        &self,
        workspace_id: Uuid,
        path: &str,
    ) -> Result<Vec<FilesystemEntry>, FilesystemError> {
        let workspace = self.workspace(workspace_id)?;
        let path = checked_path(path)?;
        let directory = open_beneath(&workspace, &path, DIRECTORY_FLAGS)?;
        let mut directory = Dir::new(directory).map_err(map_errno)?;
        let mut entries = Vec::new();

        while let Some(entry) = directory.read() {
            let entry = entry.map_err(map_errno)?;
            let name = entry.file_name();
            if is_dot(name) {
                continue;
            }
            if entries.len() == MAX_DIRECTORY_ENTRIES {
                return Err(FilesystemError::LimitExceeded);
            }
            let name_string = cstr_to_string(name)?;
            let fd = openat2(
                directory.fd().map_err(map_errno)?,
                name,
                METADATA_FLAGS,
                Mode::empty(),
                RESOLVE,
            )
            .map_err(map_errno)?;
            let stat = fstat(&fd).map_err(map_errno)?;
            let (kind, size) = safe_kind_and_size(&stat)?;
            entries.push(FilesystemEntry {
                name: name_string,
                kind,
                size,
            });
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(entries)
    }

    pub fn read(
        &self,
        workspace_id: Uuid,
        path: &str,
    ) -> Result<(Vec<u8>, String), FilesystemError> {
        let workspace = self.workspace(workspace_id)?;
        let path = checked_path(path)?;
        let metadata = open_beneath(&workspace, &path, METADATA_FLAGS)?;
        let stat = fstat(&metadata).map_err(map_errno)?;
        let (kind, _) = safe_kind_and_size(&stat)?;
        if kind != FilesystemEntryKind::File {
            return Err(FilesystemError::UnsafeEntry);
        }
        let fd = open_beneath(&workspace, &path, READ_FLAGS)?;
        read_regular(fd)
    }

    pub fn metadata(
        &self,
        workspace_id: Uuid,
        path: &str,
    ) -> Result<FilesystemMetadata, FilesystemError> {
        let workspace = self.workspace(workspace_id)?;
        let path = checked_path(path)?;
        let fd = open_beneath(&workspace, &path, METADATA_FLAGS)?;
        metadata_for_fd(&fd)
    }

    pub fn search(
        &self,
        workspace_id: Uuid,
        path: &str,
        query: &str,
    ) -> Result<Vec<FilesystemSearchMatch>, FilesystemError> {
        if query.is_empty()
            || query.len() > MAX_FILESYSTEM_SEARCH_QUERY_BYTES
            || query.as_bytes().contains(&0)
        {
            return Err(FilesystemError::InvalidPath);
        }
        let workspace = self.workspace(workspace_id)?;
        let path = checked_path(path)?;
        let directory = open_beneath(&workspace, &path, DIRECTORY_FLAGS)?;
        let prefix = if path == Path::new(".") {
            String::new()
        } else {
            path.to_str()
                .ok_or(FilesystemError::InvalidPath)?
                .to_owned()
        };
        let mut scanned = 0_usize;
        let mut encoded_bytes = 0_usize;
        let mut matches = Vec::new();
        search_directory(
            directory,
            &prefix,
            query,
            0,
            &mut scanned,
            &mut encoded_bytes,
            &mut matches,
        )?;
        matches.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(matches)
    }

    pub fn write(
        &self,
        workspace_id: Uuid,
        path: &str,
        bytes: &[u8],
    ) -> Result<String, FilesystemError> {
        self.atomic_write(workspace_id, path, None, bytes)
    }

    pub fn patch(
        &self,
        workspace_id: Uuid,
        path: &str,
        expected_sha256: &str,
        bytes: &[u8],
    ) -> Result<String, FilesystemError> {
        if expected_sha256.len() != 64
            || !expected_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(FilesystemError::InvalidPath);
        }
        self.atomic_write(workspace_id, path, Some(expected_sha256), bytes)
    }

    pub fn mkdir(&self, workspace_id: Uuid, path: &str) -> Result<(), FilesystemError> {
        let workspace = self.workspace(workspace_id)?;
        let (parent, name) = open_parent(&workspace, path)?;
        // Group/other rx: the sandbox container (subuid-mapped uid) must be
        // able to traverse directories the daemon creates.
        mkdirat(
            &parent,
            &name,
            Mode::RWXU | Mode::RGRP | Mode::ROTH | Mode::XGRP | Mode::XOTH,
        )
        .map_err(map_errno)?;
        let validation = openat2(&parent, &name, METADATA_FLAGS, Mode::empty(), RESOLVE)
            .map_err(map_errno)
            .and_then(|fd| {
                ensure_directory(&fd)?;
                let directory = openat2(&parent, &name, DIRECTORY_FLAGS, Mode::empty(), RESOLVE)
                    .map_err(map_errno)?;
                let mut scanned = 0;
                validate_tree(directory, 0, &mut scanned)?;
                Ok(())
            });
        if let Err(error) = validation {
            quarantine_and_remove(&parent, &name, &self.staging_fd)
                .map_err(|_| FilesystemError::ContainmentFailure)?;
            return Err(error);
        }
        fsync(&parent).map_err(errno_io)
    }

    pub fn move_entry(
        &self,
        workspace_id: Uuid,
        from: &str,
        to: &str,
    ) -> Result<(), FilesystemError> {
        let workspace = self.workspace(workspace_id)?;
        let source_path = checked_path(from)?;
        let destination_path = checked_path(to)?;
        if source_path == Path::new(".") {
            return Err(FilesystemError::InvalidPath);
        }
        let source = open_beneath(&workspace, &source_path, METADATA_FLAGS)?;
        let stat = fstat(&source).map_err(map_errno)?;
        let (kind, _) = safe_kind_and_size(&stat)?;
        let source_identity = identity(&stat);
        if kind == FilesystemEntryKind::Directory {
            if destination_path.starts_with(&source_path) {
                return Err(FilesystemError::InvalidPath);
            }
            let source_directory = open_beneath(&workspace, &source_path, DIRECTORY_FLAGS)?;
            let mut scanned = 0;
            validate_tree(source_directory, 0, &mut scanned)?;
        }

        let (source_parent, source_name) = open_parent(&workspace, from)?;
        let (destination_parent, destination_name) = open_parent(&workspace, to)?;
        renameat_with(
            &source_parent,
            &source_name,
            &destination_parent,
            &destination_name,
            RenameFlags::NOREPLACE,
        )
        .map_err(map_errno)?;
        if let Err(error) = verify_published(
            &destination_parent,
            &destination_name,
            source_identity,
            kind,
        ) {
            quarantine_and_remove(&destination_parent, &destination_name, &self.staging_fd)
                .map_err(|_| FilesystemError::ContainmentFailure)?;
            return Err(error);
        }
        fsync(&source_parent).map_err(errno_io)?;
        fsync(&destination_parent).map_err(errno_io)
    }

    pub fn copy(&self, workspace_id: Uuid, from: &str, to: &str) -> Result<(), FilesystemError> {
        let workspace = self.workspace(workspace_id)?;
        let source_path = checked_path(from)?;
        let destination_path = checked_path(to)?;
        let source_metadata = open_beneath(&workspace, &source_path, METADATA_FLAGS)?;
        let stat = fstat(&source_metadata).map_err(map_errno)?;
        let (kind, _) = safe_kind_and_size(&stat)?;
        if kind == FilesystemEntryKind::Directory
            && (source_path == Path::new(".") || destination_path.starts_with(&source_path))
        {
            return Err(FilesystemError::InvalidPath);
        }
        let (destination_parent, destination_name) = open_parent(&workspace, to)?;
        let stage_name = stage_name("copy");
        let mut budget = CopyBudget::default();

        let staged = match kind {
            FilesystemEntryKind::File => {
                let source = open_beneath(&workspace, &source_path, READ_FLAGS)?;
                let (bytes, _) = read_regular_bounded(source, MAX_INTERNAL_COPY_FILE_BYTES)?;
                budget.add_file(bytes.len())?;
                let mode = safe_copy_mode(stat.st_mode);
                create_staged_file(
                    &self.staging_fd,
                    &stage_name,
                    &bytes,
                    MAX_INTERNAL_COPY_FILE_BYTES,
                    mode,
                )
                .map(|(_, identity)| identity)
            }
            FilesystemEntryKind::Directory => {
                mkdirat(
                    &self.staging_fd,
                    &stage_name,
                    Mode::RWXU | Mode::RGRP | Mode::ROTH | Mode::XGRP | Mode::XOTH,
                )
                .map_err(map_errno)?;
                (|| {
                    let source = open_beneath(&workspace, &source_path, DIRECTORY_FLAGS)?;
                    let destination = openat2(
                        &self.staging_fd,
                        &stage_name,
                        DIRECTORY_FLAGS,
                        Mode::empty(),
                        RESOLVE,
                    )
                    .map_err(map_errno)?;
                    let identity = identity(&fstat(&destination).map_err(map_errno)?);
                    copy_directory(source, destination, 0, &mut budget)?;
                    Ok(identity)
                })()
            }
        };
        let staged_identity = match staged {
            Ok(identity) => identity,
            Err(error) => {
                remove_tree_best_effort(&self.staging_fd, &stage_name);
                return Err(error);
            }
        };
        if let Err(error) = verify_published(&self.staging_fd, &stage_name, staged_identity, kind) {
            remove_tree_best_effort(&self.staging_fd, &stage_name);
            return Err(error);
        }

        if let Err(error) = renameat_with(
            &self.staging_fd,
            &stage_name,
            &destination_parent,
            &destination_name,
            RenameFlags::NOREPLACE,
        ) {
            remove_tree_best_effort(&self.staging_fd, &stage_name);
            return Err(map_errno(error));
        }
        if let Err(error) = verify_published(
            &destination_parent,
            &destination_name,
            staged_identity,
            kind,
        ) {
            quarantine_and_remove(&destination_parent, &destination_name, &self.staging_fd)
                .map_err(|_| FilesystemError::ContainmentFailure)?;
            return Err(error);
        }
        fsync(&destination_parent).map_err(errno_io)
    }

    pub fn delete(&self, workspace_id: Uuid, path: &str) -> Result<(), FilesystemError> {
        let workspace = self.workspace(workspace_id)?;
        let (parent, name) = open_parent(&workspace, path)?;
        let target =
            openat2(&parent, &name, METADATA_FLAGS, Mode::empty(), RESOLVE).map_err(map_errno)?;
        let stat = fstat(&target).map_err(map_errno)?;
        let (kind, _) = safe_kind_and_size(&stat)?;
        let target_identity = identity(&stat);
        let stage_name = stage_name("delete");
        renameat_with(
            &parent,
            &name,
            &self.staging_fd,
            &stage_name,
            RenameFlags::NOREPLACE,
        )
        .map_err(map_errno)?;
        if let Err(error) = verify_published(&self.staging_fd, &stage_name, target_identity, kind) {
            remove_tree_best_effort(&self.staging_fd, &stage_name);
            return Err(error);
        }
        remove_tree(&self.staging_fd, &stage_name)?;
        fsync(&parent).map_err(errno_io)
    }

    fn workspace(&self, workspace_id: Uuid) -> Result<OwnedFd, FilesystemError> {
        if workspace_id.is_nil() {
            return Err(FilesystemError::InvalidPath);
        }
        openat2(
            self.root_fd.as_fd(),
            format!(
                "{}/{}",
                workspace_volume_name(workspace_id),
                VOLUME_DATA_DIRECTORY
            ),
            DIRECTORY_FLAGS,
            Mode::empty(),
            RESOLVE,
        )
        .map_err(|error| match error {
            Errno::NOENT => FilesystemError::WorkspaceNotFound,
            _ => map_errno(error),
        })
    }

    fn atomic_write(
        &self,
        workspace_id: Uuid,
        path: &str,
        expected_sha256: Option<&str>,
        bytes: &[u8],
    ) -> Result<String, FilesystemError> {
        if bytes.len() > MAX_FILE_PAYLOAD_BYTES {
            return Err(FilesystemError::LimitExceeded);
        }
        let workspace = self.workspace(workspace_id)?;
        let (parent, name) = open_parent(&workspace, path)?;

        match openat2(&parent, &name, METADATA_FLAGS, Mode::empty(), RESOLVE) {
            Ok(existing) => {
                let stat = fstat(&existing).map_err(map_errno)?;
                let (kind, _) = safe_kind_and_size(&stat)?;
                if kind != FilesystemEntryKind::File {
                    return Err(FilesystemError::UnsafeEntry);
                }
            }
            Err(Errno::NOENT) if expected_sha256.is_none() => {}
            Err(Errno::NOENT) => return Err(FilesystemError::NotFound),
            Err(error) => return Err(map_errno(error)),
        }

        if let Some(expected) = expected_sha256 {
            let existing =
                openat2(&parent, &name, READ_FLAGS, Mode::empty(), RESOLVE).map_err(map_errno)?;
            let (_, actual) = read_regular(existing)?;
            if actual != expected {
                return Err(FilesystemError::Conflict);
            }
        }

        let target_exists = openat2(&parent, &name, METADATA_FLAGS, Mode::empty(), RESOLVE).is_ok();
        let stage_name = stage_name("write");
        let (digest, staged_identity) = create_staged_file(
            &self.staging_fd,
            &stage_name,
            bytes,
            MAX_FILE_PAYLOAD_BYTES,
            // Group/other-read: the sandbox container runs as a subuid-mapped
            // uid (not the daemon user), so it reaches daemon-written files
            // through the "other" class.
            Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::ROTH,
        )?;
        if let Err(error) = verify_published(
            &self.staging_fd,
            &stage_name,
            staged_identity,
            FilesystemEntryKind::File,
        ) {
            remove_tree_best_effort(&self.staging_fd, &stage_name);
            return Err(error);
        }

        if target_exists {
            if let Err(error) = renameat_with(
                &self.staging_fd,
                &stage_name,
                &parent,
                &name,
                RenameFlags::EXCHANGE,
            ) {
                remove_tree_best_effort(&self.staging_fd, &stage_name);
                return Err(map_errno(error));
            }
            let validation =
                verify_published(&parent, &name, staged_identity, FilesystemEntryKind::File)
                    .and_then(|()| {
                        let displaced_metadata = openat2(
                            self.staging_fd.as_fd(),
                            &stage_name,
                            METADATA_FLAGS,
                            Mode::empty(),
                            RESOLVE,
                        )
                        .map_err(map_errno)?;
                        let displaced_stat = fstat(&displaced_metadata).map_err(map_errno)?;
                        let (displaced_kind, _) = safe_kind_and_size(&displaced_stat)?;
                        if displaced_kind != FilesystemEntryKind::File {
                            return Err(FilesystemError::UnsafeEntry);
                        }
                        if let Some(expected) = expected_sha256 {
                            let displaced = openat2(
                                self.staging_fd.as_fd(),
                                &stage_name,
                                READ_FLAGS,
                                Mode::empty(),
                                RESOLVE,
                            )
                            .map_err(map_errno)?;
                            let (_, actual) = read_regular(displaced)?;
                            if actual != expected {
                                return Err(FilesystemError::Conflict);
                            }
                        }
                        Ok(())
                    });
            if let Err(error) = validation {
                let rollback = renameat_with(
                    &self.staging_fd,
                    &stage_name,
                    &parent,
                    &name,
                    RenameFlags::EXCHANGE,
                );
                remove_tree_best_effort(&self.staging_fd, &stage_name);
                if rollback.is_ok() && validate_named_entry(&parent, &name).is_err() {
                    quarantine_and_remove(&parent, &name, &self.staging_fd)
                        .map_err(|_| FilesystemError::ContainmentFailure)?;
                }
                return if rollback.is_ok() {
                    Err(error)
                } else {
                    Err(FilesystemError::UnsafeEntry)
                };
            }
            remove_tree(&self.staging_fd, &stage_name)?;
        } else {
            if let Err(error) = renameat_with(
                &self.staging_fd,
                &stage_name,
                &parent,
                &name,
                RenameFlags::NOREPLACE,
            ) {
                remove_tree_best_effort(&self.staging_fd, &stage_name);
                return Err(map_errno(error));
            }
            if let Err(error) =
                verify_published(&parent, &name, staged_identity, FilesystemEntryKind::File)
            {
                quarantine_and_remove(&parent, &name, &self.staging_fd)
                    .map_err(|_| FilesystemError::ContainmentFailure)?;
                return Err(error);
            }
        }
        fsync(&parent).map_err(errno_io)?;
        fsync(self.staging_fd.as_fd()).map_err(errno_io)?;
        Ok(digest)
    }
}

impl WorkspaceResolver {
    pub fn expected_mountpoint(&self, workspace_id: Uuid) -> PathBuf {
        self.root_path
            .join(workspace_volume_name(workspace_id))
            .join(VOLUME_DATA_DIRECTORY)
    }

    pub fn verify_volume(
        &self,
        workspace_id: Uuid,
        driver: &str,
        options: &str,
        mountpoint: &str,
        expected: &WorkspaceStorageIdentity,
    ) -> Result<(), FilesystemError> {
        let volume_name = workspace_volume_name(workspace_id);
        if workspace_id.is_nil()
            || driver != "local"
            || options != "{}"
            || expected.volume_name != volume_name
            || mountpoint.as_bytes().contains(&0)
            || Path::new(mountpoint) != self.expected_mountpoint(workspace_id)
        {
            return Err(FilesystemError::UnsafeEntry);
        }
        let fd = openat2(
            self.root_fd.as_fd(),
            format!("{volume_name}/{VOLUME_DATA_DIRECTORY}"),
            DIRECTORY_FLAGS,
            Mode::empty(),
            RESOLVE,
        )
        .map_err(map_errno)?;
        let stat = fstat(&fd).map_err(map_errno)?;
        if stat.st_dev != expected.device || stat.st_ino != expected.inode {
            return Err(FilesystemError::UnsafeEntry);
        }
        Ok(())
    }
}

impl RecoveryStore {
    pub fn persist(&self, record: &RecoveryRecord) -> Result<(), FilesystemError> {
        if record.workspace_id.is_nil()
            || record.operation_id.is_nil()
            || record.containers.is_empty()
            || record
                .containers
                .iter()
                .any(|name| !valid_container_name(name))
        {
            return Err(FilesystemError::InvalidPath);
        }
        let encoded = format!("{}\n{}", record.operation_id, record.containers.join("\n"));
        if encoded.len() > MAX_RECOVERY_RECORD_BYTES {
            return Err(FilesystemError::LimitExceeded);
        }
        let temporary = stage_name("recovery");
        create_staged_file(
            &self.directory,
            &temporary,
            encoded.as_bytes(),
            MAX_RECOVERY_RECORD_BYTES,
            Mode::RUSR | Mode::WUSR,
        )?;
        let target = record.workspace_id.to_string();
        if let Err(error) = renameat(&self.directory, &temporary, &self.directory, &target) {
            remove_tree_best_effort(&self.directory, &temporary);
            return Err(map_errno(error));
        }
        fsync(self.directory.as_fd()).map_err(errno_io)
    }

    pub fn list(&self) -> Result<Vec<RecoveryRecord>, FilesystemError> {
        let mut directory = Dir::read_from(self.directory.as_fd()).map_err(map_errno)?;
        let mut records = Vec::new();
        while let Some(entry) = directory.read() {
            let entry = entry.map_err(map_errno)?;
            if is_dot(entry.file_name()) {
                continue;
            }
            let name = cstr_to_string(entry.file_name())?;
            let workspace_id = Uuid::parse_str(&name).map_err(|_| FilesystemError::UnsafeEntry)?;
            let fd = openat2(
                directory.fd().map_err(map_errno)?,
                entry.file_name(),
                READ_FLAGS,
                Mode::empty(),
                RESOLVE,
            )
            .map_err(map_errno)?;
            let (bytes, _) = read_regular_bounded(fd, MAX_RECOVERY_RECORD_BYTES)?;
            let encoded = String::from_utf8(bytes).map_err(|_| FilesystemError::UnsafeEntry)?;
            let mut lines = encoded.lines();
            let operation_id = lines
                .next()
                .and_then(|value| Uuid::parse_str(value).ok())
                .filter(|value| !value.is_nil())
                .ok_or(FilesystemError::UnsafeEntry)?;
            let containers = lines.map(str::to_owned).collect::<Vec<_>>();
            if containers.is_empty() || containers.iter().any(|item| !valid_container_name(item)) {
                return Err(FilesystemError::UnsafeEntry);
            }
            records.push(RecoveryRecord {
                workspace_id,
                operation_id,
                containers,
            });
        }
        records.sort_by_key(|record| record.workspace_id);
        Ok(records)
    }

    pub fn clear(&self, workspace_id: Uuid, operation_id: Uuid) -> Result<(), FilesystemError> {
        let current = self
            .list()?
            .into_iter()
            .find(|record| record.workspace_id == workspace_id)
            .ok_or(FilesystemError::NotFound)?;
        if current.operation_id != operation_id {
            return Err(FilesystemError::Conflict);
        }
        unlinkat(
            self.directory.as_fd(),
            workspace_id.to_string(),
            AtFlags::empty(),
        )
        .map_err(map_errno)?;
        fsync(self.directory.as_fd()).map_err(errno_io)
    }
}

fn valid_container_name(name: &str) -> bool {
    name.strip_prefix("gobrowse-")
        .and_then(|value| Uuid::parse_str(value).ok())
        .is_some()
}

fn checked_path(value: &str) -> Result<PathBuf, FilesystemError> {
    let path = validate_workspace_path(value).map_err(|_| FilesystemError::InvalidPath)?;
    let normalized = path
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(component) => Some(component),
            std::path::Component::CurDir => None,
            _ => None,
        })
        .collect::<PathBuf>();
    if normalized.as_os_str().is_empty() {
        Ok(PathBuf::from("."))
    } else {
        Ok(normalized)
    }
}

fn open_parent(workspace: &OwnedFd, value: &str) -> Result<(OwnedFd, OsString), FilesystemError> {
    let path = checked_path(value)?;
    if path == Path::new(".") || value.ends_with('/') || value.ends_with("/.") {
        return Err(FilesystemError::InvalidPath);
    }
    let name = path
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or(FilesystemError::InvalidPath)?
        .to_owned();
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = open_beneath(workspace, parent, DIRECTORY_FLAGS)?;
    Ok((parent, name))
}

fn open_beneath(
    directory: &OwnedFd,
    path: &Path,
    flags: OFlags,
) -> Result<OwnedFd, FilesystemError> {
    openat2(directory, path, flags, Mode::empty(), RESOLVE).map_err(map_errno)
}

fn ensure_directory(fd: &OwnedFd) -> Result<(), FilesystemError> {
    let stat = fstat(fd).map_err(map_errno)?;
    if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
        Ok(())
    } else {
        Err(FilesystemError::UnsafeEntry)
    }
}

fn safe_kind_and_size(
    stat: &rustix::fs::Stat,
) -> Result<(FilesystemEntryKind, u64), FilesystemError> {
    match FileType::from_raw_mode(stat.st_mode) {
        FileType::RegularFile => {
            if stat.st_nlink != 1 {
                return Err(FilesystemError::UnsafeEntry);
            }
            let size = u64::try_from(stat.st_size).map_err(|_| FilesystemError::UnsafeEntry)?;
            Ok((FilesystemEntryKind::File, size))
        }
        FileType::Directory => Ok((FilesystemEntryKind::Directory, 0)),
        _ => Err(FilesystemError::UnsafeEntry),
    }
}

fn metadata_for_fd(fd: &OwnedFd) -> Result<FilesystemMetadata, FilesystemError> {
    let stat = fstat(fd).map_err(map_errno)?;
    let (kind, size) = safe_kind_and_size(&stat)?;
    Ok(FilesystemMetadata {
        kind,
        size,
        mode: stat.st_mode & 0o7777,
        modified_unix_seconds: stat.st_mtime,
    })
}

fn read_regular(fd: OwnedFd) -> Result<(Vec<u8>, String), FilesystemError> {
    read_regular_bounded(fd, MAX_FILE_PAYLOAD_BYTES)
}

fn read_regular_bounded(fd: OwnedFd, maximum: usize) -> Result<(Vec<u8>, String), FilesystemError> {
    let stat = fstat(&fd).map_err(map_errno)?;
    let (kind, size) = safe_kind_and_size(&stat)?;
    if kind != FilesystemEntryKind::File {
        return Err(FilesystemError::UnsafeEntry);
    }
    if size > maximum as u64 {
        return Err(FilesystemError::LimitExceeded);
    }
    let mut file = File::from(fd);
    let mut bytes = Vec::with_capacity(size as usize);
    Read::by_ref(&mut file)
        .take(maximum as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(FilesystemError::Io)?;
    if bytes.len() > maximum {
        return Err(FilesystemError::LimitExceeded);
    }
    let digest = sha256(&bytes);
    Ok((bytes, digest))
}

fn create_staged_file(
    parent: &OwnedFd,
    name: &OsStr,
    bytes: &[u8],
    maximum: usize,
    mode: Mode,
) -> Result<(String, FileIdentity), FilesystemError> {
    if bytes.len() > maximum {
        return Err(FilesystemError::LimitExceeded);
    }
    let fd = openat2(parent, name, CREATE_FLAGS, mode, RESOLVE).map_err(map_errno)?;
    fchmod(&fd, mode).map_err(map_errno)?;
    let identity = identity(&fstat(&fd).map_err(map_errno)?);
    let mut file = File::from(fd);
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = unlinkat(parent, name, AtFlags::empty());
        return Err(FilesystemError::Io(error));
    }
    Ok((sha256(bytes), identity))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

fn identity(stat: &rustix::fs::Stat) -> FileIdentity {
    FileIdentity {
        device: stat.st_dev,
        inode: stat.st_ino,
    }
}

fn safe_copy_mode(mode: u32) -> Mode {
    // Group/other-read (plus preserved exec bits): the sandbox container runs
    // as a subuid-mapped uid and must be able to read copied files.
    Mode::from_raw_mode(0o644 | (mode & 0o111))
}

#[derive(Default)]
struct CopyBudget {
    entries: usize,
    bytes: u64,
}

impl CopyBudget {
    fn add_entry(&mut self) -> Result<(), FilesystemError> {
        self.entries += 1;
        if self.entries > MAX_INTERNAL_COPY_ENTRIES {
            Err(FilesystemError::LimitExceeded)
        } else {
            Ok(())
        }
    }

    fn add_file(&mut self, bytes: usize) -> Result<(), FilesystemError> {
        self.add_entry()?;
        self.bytes = self
            .bytes
            .checked_add(bytes as u64)
            .ok_or(FilesystemError::LimitExceeded)?;
        if self.bytes > MAX_INTERNAL_COPY_TOTAL_BYTES {
            Err(FilesystemError::LimitExceeded)
        } else {
            Ok(())
        }
    }
}

fn verify_published(
    parent: &OwnedFd,
    name: &OsStr,
    expected: FileIdentity,
    expected_kind: FilesystemEntryKind,
) -> Result<(), FilesystemError> {
    let fd = openat2(parent, name, METADATA_FLAGS, Mode::empty(), RESOLVE).map_err(map_errno)?;
    let stat = fstat(&fd).map_err(map_errno)?;
    let (kind, _) = safe_kind_and_size(&stat)?;
    if identity(&stat) != expected || kind != expected_kind {
        return Err(FilesystemError::UnsafeEntry);
    }
    if kind == FilesystemEntryKind::Directory {
        let directory =
            openat2(parent, name, DIRECTORY_FLAGS, Mode::empty(), RESOLVE).map_err(map_errno)?;
        let mut scanned = 0;
        validate_tree(directory, 0, &mut scanned)?;
    }
    Ok(())
}

fn validate_named_entry(parent: &OwnedFd, name: &OsStr) -> Result<(), FilesystemError> {
    let fd = openat2(parent, name, METADATA_FLAGS, Mode::empty(), RESOLVE).map_err(map_errno)?;
    let stat = fstat(&fd).map_err(map_errno)?;
    let (kind, _) = safe_kind_and_size(&stat)?;
    if kind == FilesystemEntryKind::Directory {
        let directory =
            openat2(parent, name, DIRECTORY_FLAGS, Mode::empty(), RESOLVE).map_err(map_errno)?;
        let mut scanned = 0;
        validate_tree(directory, 0, &mut scanned)?;
    }
    Ok(())
}

fn search_directory(
    directory: OwnedFd,
    prefix: &str,
    query: &str,
    depth: usize,
    scanned: &mut usize,
    encoded_bytes: &mut usize,
    matches: &mut Vec<FilesystemSearchMatch>,
) -> Result<(), FilesystemError> {
    if depth >= MAX_WORKSPACE_PATH_COMPONENTS {
        return Err(FilesystemError::LimitExceeded);
    }
    let mut directory = Dir::new(directory).map_err(map_errno)?;
    while let Some(entry) = directory.read() {
        let entry = entry.map_err(map_errno)?;
        let name = entry.file_name();
        if is_dot(name) {
            continue;
        }
        *scanned += 1;
        if *scanned > MAX_FILESYSTEM_SCANNED_ENTRIES {
            return Err(FilesystemError::LimitExceeded);
        }
        let name_string = cstr_to_string(name)?;
        let entry_path = if prefix.is_empty() {
            name_string.clone()
        } else {
            format!("{prefix}/{name_string}")
        };
        if entry_path.len() > MAX_WORKSPACE_PATH_BYTES {
            return Err(FilesystemError::LimitExceeded);
        }
        let metadata_fd = openat2(
            directory.fd().map_err(map_errno)?,
            name,
            METADATA_FLAGS,
            Mode::empty(),
            RESOLVE,
        )
        .map_err(map_errno)?;
        let stat = fstat(&metadata_fd).map_err(map_errno)?;
        let (kind, size) = safe_kind_and_size(&stat)?;
        if name_string.contains(query) {
            if matches.len() == MAX_FILESYSTEM_SEARCH_RESULTS {
                return Err(FilesystemError::LimitExceeded);
            }
            let found = FilesystemSearchMatch {
                path: entry_path.clone(),
                kind,
                size,
            };
            *encoded_bytes += serde_json::to_vec(&found)
                .map_err(|_| FilesystemError::LimitExceeded)?
                .len()
                + 1;
            if *encoded_bytes > MAX_FILESYSTEM_SEARCH_ENCODED_BYTES {
                return Err(FilesystemError::LimitExceeded);
            }
            matches.push(found);
        }
        if kind == FilesystemEntryKind::Directory {
            let child = openat2(
                directory.fd().map_err(map_errno)?,
                name,
                DIRECTORY_FLAGS,
                Mode::empty(),
                RESOLVE,
            )
            .map_err(map_errno)?;
            search_directory(
                child,
                &entry_path,
                query,
                depth + 1,
                scanned,
                encoded_bytes,
                matches,
            )?;
        }
    }
    Ok(())
}

fn validate_tree(
    directory: OwnedFd,
    depth: usize,
    scanned: &mut usize,
) -> Result<(), FilesystemError> {
    if depth >= MAX_WORKSPACE_PATH_COMPONENTS {
        return Err(FilesystemError::LimitExceeded);
    }
    let mut directory = Dir::new(directory).map_err(map_errno)?;
    while let Some(entry) = directory.read() {
        let entry = entry.map_err(map_errno)?;
        let name = entry.file_name();
        if is_dot(name) {
            continue;
        }
        *scanned += 1;
        if *scanned > MAX_FILESYSTEM_SCANNED_ENTRIES {
            return Err(FilesystemError::LimitExceeded);
        }
        cstr_to_string(name)?;
        let fd = openat2(
            directory.fd().map_err(map_errno)?,
            name,
            METADATA_FLAGS,
            Mode::empty(),
            RESOLVE,
        )
        .map_err(map_errno)?;
        let stat = fstat(&fd).map_err(map_errno)?;
        let (kind, _) = safe_kind_and_size(&stat)?;
        if kind == FilesystemEntryKind::Directory {
            let child = openat2(
                directory.fd().map_err(map_errno)?,
                name,
                DIRECTORY_FLAGS,
                Mode::empty(),
                RESOLVE,
            )
            .map_err(map_errno)?;
            validate_tree(child, depth + 1, scanned)?;
        }
    }
    Ok(())
}

fn copy_directory(
    source: OwnedFd,
    destination: OwnedFd,
    depth: usize,
    budget: &mut CopyBudget,
) -> Result<(), FilesystemError> {
    if depth >= MAX_WORKSPACE_PATH_COMPONENTS {
        return Err(FilesystemError::LimitExceeded);
    }
    let mut source = Dir::new(source).map_err(map_errno)?;
    while let Some(entry) = source.read() {
        let entry = entry.map_err(map_errno)?;
        let name = entry.file_name();
        if is_dot(name) {
            continue;
        }
        cstr_to_string(name)?;
        let metadata_fd = openat2(
            source.fd().map_err(map_errno)?,
            name,
            METADATA_FLAGS,
            Mode::empty(),
            RESOLVE,
        )
        .map_err(map_errno)?;
        let stat = fstat(&metadata_fd).map_err(map_errno)?;
        let (kind, _) = safe_kind_and_size(&stat)?;
        match kind {
            FilesystemEntryKind::File => {
                let source_file = openat2(
                    source.fd().map_err(map_errno)?,
                    name,
                    READ_FLAGS,
                    Mode::empty(),
                    RESOLVE,
                )
                .map_err(map_errno)?;
                let (bytes, _) = read_regular_bounded(source_file, MAX_INTERNAL_COPY_FILE_BYTES)?;
                budget.add_file(bytes.len())?;
                create_staged_file(
                    &destination,
                    OsStr::from_bytes(name.to_bytes()),
                    &bytes,
                    MAX_INTERNAL_COPY_FILE_BYTES,
                    safe_copy_mode(stat.st_mode),
                )?;
            }
            FilesystemEntryKind::Directory => {
                budget.add_entry()?;
                mkdirat(
                    &destination,
                    name,
                    Mode::RWXU | Mode::RGRP | Mode::ROTH | Mode::XGRP | Mode::XOTH,
                )
                .map_err(map_errno)?;
                let source_child = openat2(
                    source.fd().map_err(map_errno)?,
                    name,
                    DIRECTORY_FLAGS,
                    Mode::empty(),
                    RESOLVE,
                )
                .map_err(map_errno)?;
                let destination_child =
                    openat2(&destination, name, DIRECTORY_FLAGS, Mode::empty(), RESOLVE)
                        .map_err(map_errno)?;
                copy_directory(source_child, destination_child, depth + 1, budget)?;
            }
        }
    }
    fsync(&destination).map_err(errno_io)
}

fn remove_tree_best_effort(parent: &OwnedFd, name: &OsStr) {
    let directory = openat2(parent, name, DIRECTORY_FLAGS, Mode::empty(), RESOLVE);
    if let Ok(directory) = directory {
        if let Ok(mut directory) = Dir::new(directory) {
            while let Some(Ok(entry)) = directory.read() {
                if is_dot(entry.file_name()) {
                    continue;
                }
                let Ok(fd) = directory.fd() else {
                    break;
                };
                remove_tree_best_effort_cstr(fd, entry.file_name());
            }
        }
        let _ = unlinkat(parent, name, AtFlags::REMOVEDIR);
    } else {
        let _ = unlinkat(parent, name, AtFlags::empty());
    }
}

fn remove_tree(parent: &OwnedFd, name: &OsStr) -> Result<(), FilesystemError> {
    let directory = openat2(parent, name, DIRECTORY_FLAGS, Mode::empty(), RESOLVE);
    if let Ok(directory) = directory {
        let mut directory = Dir::new(directory).map_err(map_errno)?;
        while let Some(entry) = directory.read() {
            let entry = entry.map_err(map_errno)?;
            if is_dot(entry.file_name()) {
                continue;
            }
            remove_tree_cstr(directory.fd().map_err(map_errno)?, entry.file_name())?;
        }
        unlinkat(parent, name, AtFlags::REMOVEDIR).map_err(map_errno)
    } else {
        unlinkat(parent, name, AtFlags::empty()).map_err(map_errno)
    }
}

fn remove_tree_cstr(parent: impl AsFd, name: &CStr) -> Result<(), FilesystemError> {
    let directory = openat2(
        parent.as_fd(),
        name,
        DIRECTORY_FLAGS,
        Mode::empty(),
        RESOLVE,
    );
    if let Ok(directory) = directory {
        let mut directory = Dir::new(directory).map_err(map_errno)?;
        while let Some(entry) = directory.read() {
            let entry = entry.map_err(map_errno)?;
            if is_dot(entry.file_name()) {
                continue;
            }
            remove_tree_cstr(directory.fd().map_err(map_errno)?, entry.file_name())?;
        }
        unlinkat(parent.as_fd(), name, AtFlags::REMOVEDIR).map_err(map_errno)
    } else {
        unlinkat(parent.as_fd(), name, AtFlags::empty()).map_err(map_errno)
    }
}

fn clear_directory(directory: &OwnedFd) -> Result<(), FilesystemError> {
    let mut entries = Dir::read_from(directory).map_err(map_errno)?;
    while let Some(entry) = entries.read() {
        let entry = entry.map_err(map_errno)?;
        if is_dot(entry.file_name()) {
            continue;
        }
        remove_tree_cstr(entries.fd().map_err(map_errno)?, entry.file_name())?;
    }
    fsync(directory).map_err(errno_io)
}

fn quarantine_and_remove(
    parent: &OwnedFd,
    name: &OsStr,
    staging: &OwnedFd,
) -> Result<(), FilesystemError> {
    let quarantine = stage_name("quarantine");
    renameat_with(parent, name, staging, &quarantine, RenameFlags::NOREPLACE).map_err(map_errno)?;
    remove_tree(staging, &quarantine)
}

fn remove_tree_best_effort_cstr(parent: impl AsFd, name: &CStr) {
    let directory = openat2(
        parent.as_fd(),
        name,
        DIRECTORY_FLAGS,
        Mode::empty(),
        RESOLVE,
    );
    if let Ok(directory) = directory {
        if let Ok(mut directory) = Dir::new(directory) {
            while let Some(Ok(entry)) = directory.read() {
                if is_dot(entry.file_name()) {
                    continue;
                }
                let Ok(fd) = directory.fd() else {
                    break;
                };
                remove_tree_best_effort_cstr(fd, entry.file_name());
            }
        }
        let _ = unlinkat(parent.as_fd(), name, AtFlags::REMOVEDIR);
    } else {
        let _ = unlinkat(parent.as_fd(), name, AtFlags::empty());
    }
}

fn stage_name(operation: &str) -> OsString {
    format!(".gobrowse-{operation}-{}", Uuid::new_v4()).into()
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn is_dot(name: &CStr) -> bool {
    matches!(name.to_bytes(), b"." | b"..")
}

fn cstr_to_string(name: &CStr) -> Result<String, FilesystemError> {
    name.to_str()
        .map(str::to_owned)
        .map_err(|_| FilesystemError::UnsafeEntry)
}

fn map_errno(error: Errno) -> FilesystemError {
    match error {
        Errno::NOENT => FilesystemError::NotFound,
        Errno::EXIST | Errno::NOTEMPTY => FilesystemError::Conflict,
        Errno::LOOP | Errno::XDEV | Errno::NOTDIR | Errno::ISDIR => FilesystemError::UnsafeEntry,
        _ => errno_io(error),
    }
}

fn errno_io(error: Errno) -> FilesystemError {
    FilesystemError::Io(error.into())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt, symlink},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
    };

    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("gobrowse-sandboxd-{}", Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            // Filesystem::new requires a private root; pin the mode so the
            // tests are immune to the ambient umask.
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn spawn_symlink_swap_attacker(
        workspace_path: &Path,
        other_path: &Path,
        stop: Arc<AtomicBool>,
    ) -> thread::JoinHandle<()> {
        let stable = workspace_path.join("stable");
        let parked = workspace_path.join("parked");
        let other = other_path.to_path_buf();
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if fs::rename(&stable, &parked).is_ok() {
                    let _ = symlink(&other, &stable);
                    let _ = fs::remove_file(&stable);
                    let _ = fs::rename(&parked, &stable);
                }
            }
        })
    }

    #[test]
    fn trusted_root_must_be_absolute_preexisting_and_have_private_staging() {
        assert!(matches!(
            Filesystem::new("relative-workspace-root"),
            Err(FilesystemError::InvalidPath)
        ));
        let missing = std::env::temp_dir().join(format!("missing-{}", Uuid::new_v4()));
        assert!(matches!(
            Filesystem::new(&missing),
            Err(FilesystemError::Io(_))
        ));
        let writable_root = TestDirectory::new();
        fs::set_permissions(&writable_root.0, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(matches!(
            Filesystem::new(&writable_root.0),
            Err(FilesystemError::UnsafeEntry)
        ));

        let root = TestDirectory::new();
        let outside = TestDirectory::new();
        fs::write(outside.0.join("marker"), b"outside").unwrap();
        symlink(&outside.0, root.0.join(STAGING_DIRECTORY)).unwrap();
        assert!(matches!(
            Filesystem::new(&root.0),
            Err(FilesystemError::UnsafeEntry)
        ));
        assert_eq!(fs::read(outside.0.join("marker")).unwrap(), b"outside");

        let clean_root = TestDirectory::new();
        let staging = clean_root.0.join(STAGING_DIRECTORY);
        fs::create_dir(&staging).unwrap();
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(staging.join("leftover"), b"stale").unwrap();
        let filesystem = Filesystem::new(&clean_root.0).unwrap();
        assert_eq!(fs::read_dir(&staging).unwrap().count(), 0);
        let workspace = Uuid::new_v4();
        filesystem.ensure_workspace(workspace).unwrap();
        assert!(
            filesystem
                .list(workspace, ".")
                .unwrap()
                .iter()
                .all(|entry| entry.name != STAGING_DIRECTORY)
        );
    }

    #[test]
    fn descriptor_operations_are_bounded_and_workspace_scoped() {
        let root = TestDirectory::new();
        let filesystem = Filesystem::new(&root.0).unwrap();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        filesystem.ensure_workspace(first).unwrap();
        filesystem.ensure_workspace(second).unwrap();
        let storage = filesystem.workspace_storage(first).unwrap();
        let resolver = filesystem.resolver();
        let mountpoint = resolver.expected_mountpoint(first);
        assert!(
            resolver
                .verify_volume(first, "local", "{}", mountpoint.to_str().unwrap(), &storage)
                .is_ok()
        );
        for (driver, options, mountpoint) in [
            ("overlay", "{}", mountpoint.to_str().unwrap()),
            (
                "local",
                "{\"device\":\"/tmp/escape\"}",
                mountpoint.to_str().unwrap(),
            ),
            ("local", "{}", "/tmp/escape"),
        ] {
            assert!(
                resolver
                    .verify_volume(first, driver, options, mountpoint, &storage)
                    .is_err()
            );
        }
        filesystem.mkdir(first, "src").unwrap();
        let hash = filesystem.write(first, "src/main.rs", b"first").unwrap();

        assert_eq!(filesystem.read(first, "src/main.rs").unwrap().0, b"first");
        assert_eq!(hash, sha256(b"first"));
        assert!(matches!(
            filesystem.read(second, "src/main.rs"),
            Err(FilesystemError::NotFound)
        ));
        for invalid in ["../second/secret", "/etc/passwd", "nul\0path"] {
            assert!(matches!(
                filesystem.read(first, invalid),
                Err(FilesystemError::InvalidPath)
            ));
        }
        assert!(matches!(
            filesystem.write(first, "large", &vec![0; MAX_FILE_PAYLOAD_BYTES + 1]),
            Err(FilesystemError::LimitExceeded)
        ));
    }

    #[test]
    fn patch_is_hash_conditional_and_writes_are_atomic() {
        let root = TestDirectory::new();
        let filesystem = Filesystem::new(&root.0).unwrap();
        let workspace = Uuid::new_v4();
        filesystem.ensure_workspace(workspace).unwrap();
        let original = filesystem.write(workspace, "file", b"old").unwrap();

        assert!(matches!(
            filesystem.patch(workspace, "file", &sha256(b"wrong"), b"new"),
            Err(FilesystemError::Conflict)
        ));
        let replacement = filesystem
            .patch(workspace, "file", &original, b"new")
            .unwrap();
        assert_eq!(replacement, sha256(b"new"));
        assert_eq!(filesystem.read(workspace, "file").unwrap().0, b"new");
    }

    #[test]
    fn list_search_metadata_copy_move_and_delete_use_descriptors() {
        let root = TestDirectory::new();
        let filesystem = Filesystem::new(&root.0).unwrap();
        let workspace = Uuid::new_v4();
        filesystem.ensure_workspace(workspace).unwrap();
        filesystem.mkdir(workspace, "src").unwrap();
        filesystem
            .write(workspace, "src/main.rs", b"fn main() {}")
            .unwrap();

        let metadata = filesystem.metadata(workspace, "src/main.rs").unwrap();
        assert_eq!(metadata.kind, FilesystemEntryKind::File);
        assert_eq!(metadata.size, 12);
        assert_eq!(filesystem.search(workspace, ".", "main").unwrap().len(), 1);
        assert!(matches!(
            filesystem.copy(workspace, "src", "src/nested"),
            Err(FilesystemError::InvalidPath)
        ));
        assert!(matches!(
            filesystem.copy(workspace, ".", "snapshot"),
            Err(FilesystemError::InvalidPath)
        ));
        filesystem.copy(workspace, "src", "copied").unwrap();
        filesystem
            .move_entry(workspace, "copied/main.rs", "moved.rs")
            .unwrap();
        assert!(matches!(
            filesystem.copy(workspace, "src/main.rs", "moved.rs"),
            Err(FilesystemError::Conflict)
        ));
        filesystem.delete(workspace, "moved.rs").unwrap();
        assert_eq!(filesystem.list(workspace, "copied").unwrap().len(), 0);
    }

    #[test]
    fn copy_has_separate_bounds_and_preserves_safe_executable_bits() {
        let root = TestDirectory::new();
        let filesystem = Filesystem::new(&root.0).unwrap();
        let workspace = Uuid::new_v4();
        let workspace_path = filesystem.ensure_workspace(workspace).unwrap();
        let source = workspace_path.join("tool");
        fs::write(&source, vec![b'x'; MAX_FILE_PAYLOAD_BYTES + 1]).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o751)).unwrap();

        assert!(matches!(
            filesystem.read(workspace, "tool"),
            Err(FilesystemError::LimitExceeded)
        ));
        filesystem.copy(workspace, "tool", "tool-copy").unwrap();
        let copied = fs::metadata(workspace_path.join("tool-copy")).unwrap();
        assert_eq!(copied.permissions().mode() & 0o111, 0o111);
        assert_eq!(copied.permissions().mode() & 0o6000, 0);
    }

    #[test]
    fn search_is_depth_first_fd_bounded_and_rejects_oversized_results() {
        let root = TestDirectory::new();
        let filesystem = Filesystem::new(&root.0).unwrap();
        let workspace = Uuid::new_v4();
        let workspace_path = filesystem.ensure_workspace(workspace).unwrap();
        for index in 0..1_100 {
            fs::create_dir(workspace_path.join(format!("directory-{index:04}"))).unwrap();
        }
        assert!(filesystem.search(workspace, ".", "absent").is_ok());

        let long_directory = "d".repeat(220);
        fs::create_dir(workspace_path.join(&long_directory)).unwrap();
        for index in 0..1_024 {
            let name = format!("match-{index:04}-{}", "x".repeat(225));
            fs::write(workspace_path.join(&long_directory).join(name), b"").unwrap();
        }
        assert!(matches!(
            filesystem.search(workspace, ".", "match-"),
            Err(FilesystemError::LimitExceeded)
        ));
    }

    #[test]
    fn symlinks_hardlinks_and_special_files_are_rejected() {
        let root = TestDirectory::new();
        let filesystem = Filesystem::new(&root.0).unwrap();
        let workspace = Uuid::new_v4();
        let workspace_path = filesystem.ensure_workspace(workspace).unwrap();
        fs::write(workspace_path.join("file"), b"data").unwrap();
        fs::hard_link(workspace_path.join("file"), workspace_path.join("hardlink")).unwrap();
        symlink("/etc/passwd", workspace_path.join("link")).unwrap();
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            workspace_path.join("fifo"),
            Mode::RUSR | Mode::WUSR,
        )
        .unwrap();

        for path in ["file", "hardlink", "link", "fifo"] {
            assert!(matches!(
                filesystem.read(workspace, path),
                Err(FilesystemError::UnsafeEntry)
            ));
        }
        assert!(matches!(
            filesystem.list(workspace, "."),
            Err(FilesystemError::UnsafeEntry)
        ));
    }

    #[test]
    fn cross_workspace_and_symlink_swap_cannot_escape_retained_descriptors() {
        let root = TestDirectory::new();
        let filesystem = Arc::new(Filesystem::new(&root.0).unwrap());
        let workspace = Uuid::new_v4();
        let other = Uuid::new_v4();
        let workspace_path = filesystem.ensure_workspace(workspace).unwrap();
        let other_path = filesystem.ensure_workspace(other).unwrap();
        fs::write(other_path.join("secret"), b"outside").unwrap();
        fs::create_dir(workspace_path.join("stable")).unwrap();
        fs::write(workspace_path.join("stable/value"), b"inside").unwrap();

        assert!(filesystem.read(workspace, "../secret").is_err());
        symlink(&other_path, workspace_path.join("escape")).unwrap();
        assert!(matches!(
            filesystem.read(workspace, "escape/secret"),
            Err(FilesystemError::UnsafeEntry)
        ));

        let stop = Arc::new(AtomicBool::new(false));
        let attacker = spawn_symlink_swap_attacker(&workspace_path, &other_path, Arc::clone(&stop));

        for _ in 0..2_000 {
            if let Ok((bytes, _)) = filesystem.read(workspace, "stable/value") {
                assert_eq!(bytes, b"inside");
            }
        }
        stop.store(true, Ordering::Relaxed);
        attacker.join().unwrap();
        assert_eq!(fs::read(other_path.join("secret")).unwrap(), b"outside");
        assert!(workspace_path.metadata().unwrap().ino() != other_path.metadata().unwrap().ino());
    }

    #[test]
    fn mutating_ops_survive_symlink_swap_without_escape() {
        let root = TestDirectory::new();
        let filesystem = Arc::new(Filesystem::new(&root.0).unwrap());
        let workspace = Uuid::new_v4();
        let other = Uuid::new_v4();
        let workspace_path = filesystem.ensure_workspace(workspace).unwrap();
        let other_path = filesystem.ensure_workspace(other).unwrap();
        fs::write(other_path.join("secret"), b"outside").unwrap();
        fs::create_dir(workspace_path.join("stable")).unwrap();
        fs::write(workspace_path.join("stable/value"), b"inside").unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let attacker = spawn_symlink_swap_attacker(&workspace_path, &other_path, Arc::clone(&stop));

        let inside_hash = sha256(b"inside");

        for _ in 0..2_000 {
            match filesystem.write(workspace, "stable/value", b"overwrite") {
                Ok(hash) => assert_eq!(hash, sha256(b"overwrite")),
                Err(e) => assert!(
                    !matches!(e, FilesystemError::LimitExceeded),
                    "write returned LimitExceeded: {e}"
                ),
            }
            match filesystem.patch(workspace, "stable/value", &inside_hash, b"patched") {
                Ok(hash) => assert_eq!(hash, sha256(b"patched")),
                Err(e) => assert!(
                    !matches!(e, FilesystemError::LimitExceeded),
                    "patch returned LimitExceeded: {e}"
                ),
            }
            match filesystem.mkdir(workspace, "stable/sub") {
                Ok(()) => {}
                Err(e) => assert!(
                    !matches!(e, FilesystemError::LimitExceeded),
                    "mkdir returned LimitExceeded: {e}"
                ),
            }
            match filesystem.move_entry(workspace, "stable/sub", "stable/moved") {
                Ok(()) => {}
                Err(e) => assert!(
                    !matches!(e, FilesystemError::LimitExceeded),
                    "move_entry returned LimitExceeded: {e}"
                ),
            }
            match filesystem.copy(workspace, "stable/value", "stable/copy") {
                Ok(()) => {}
                Err(e) => assert!(
                    !matches!(e, FilesystemError::LimitExceeded),
                    "copy returned LimitExceeded: {e}"
                ),
            }
            match filesystem.delete(workspace, "stable/copy") {
                Ok(()) => {}
                Err(e) => assert!(
                    !matches!(e, FilesystemError::LimitExceeded),
                    "delete returned LimitExceeded: {e}"
                ),
            }
        }

        stop.store(true, Ordering::Relaxed);
        attacker.join().unwrap();
        assert_eq!(fs::read(other_path.join("secret")).unwrap(), b"outside");
        assert!(workspace_path.metadata().unwrap().ino() != other_path.metadata().unwrap().ino());
    }
}
