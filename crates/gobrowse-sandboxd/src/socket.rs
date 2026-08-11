use std::{
    io::ErrorKind,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::PathBuf,
};

use tokio::net::{UnixListener, UnixStream};

use crate::DaemonError;

#[derive(Debug, Clone)]
pub struct SocketConfig {
    pub path: PathBuf,
    pub mode: u32,
    pub owner_uid: u32,
    pub allowed_peer_uid: u32,
}

impl SocketConfig {
    pub fn validate(&self) -> Result<(), DaemonError> {
        if self.path.as_os_str().is_empty()
            || self.mode & !0o660 != 0
            || self.mode & 0o600 != 0o600
            || (self.allowed_peer_uid != self.owner_uid && self.mode & 0o060 != 0o060)
        {
            return Err(DaemonError::InvalidConfiguration);
        }
        Ok(())
    }

    pub async fn bind(&self) -> Result<(UnixListener, SocketGuard), DaemonError> {
        self.validate()?;
        self.validate_parent()?;
        self.remove_owned_stale_socket().await?;

        let listener = UnixListener::bind(&self.path)?;
        let metadata = match std::fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) => {
                let _ = std::fs::remove_file(&self.path);
                return Err(DaemonError::Io(error));
            }
        };
        if !metadata.file_type().is_socket() || metadata.uid() != self.owner_uid {
            let _ = std::fs::remove_file(&self.path);
            return Err(DaemonError::InvalidConfiguration);
        }
        let guard = SocketGuard {
            path: self.path.clone(),
            owner_uid: self.owner_uid,
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(self.mode))?;
        let configured = std::fs::symlink_metadata(&self.path)?;
        if !same_owned_socket(&metadata, &configured, self.owner_uid)
            || configured.mode() & 0o777 != self.mode
        {
            return Err(DaemonError::InvalidConfiguration);
        }
        Ok((listener, guard))
    }

    pub fn validate_peer(&self, stream: &UnixStream) -> Result<(), DaemonError> {
        let credentials = stream.peer_cred()?;
        if credentials.uid() == self.allowed_peer_uid {
            Ok(())
        } else {
            Err(DaemonError::Unauthorized)
        }
    }

    fn validate_parent(&self) -> Result<(), DaemonError> {
        let parent = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or(DaemonError::InvalidConfiguration)?;
        let metadata = std::fs::symlink_metadata(parent)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() != self.owner_uid
            || metadata.mode() & 0o022 != 0
        {
            return Err(DaemonError::InvalidConfiguration);
        }
        Ok(())
    }

    async fn remove_owned_stale_socket(&self) -> Result<(), DaemonError> {
        let metadata = match std::fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(DaemonError::Io(error)),
        };
        if !metadata.file_type().is_socket() || metadata.uid() != self.owner_uid {
            return Err(DaemonError::InvalidConfiguration);
        }
        match UnixStream::connect(&self.path).await {
            Ok(_) => return Err(DaemonError::Conflict),
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::ConnectionRefused | ErrorKind::NotFound
                ) => {}
            Err(error) => return Err(DaemonError::Io(error)),
        }

        let current = std::fs::symlink_metadata(&self.path)?;
        if !same_owned_socket(&metadata, &current, self.owner_uid) {
            return Err(DaemonError::Conflict);
        }
        std::fs::remove_file(&self.path)?;
        Ok(())
    }
}

pub struct SocketGuard {
    path: PathBuf,
    owner_uid: u32,
    device: u64,
    inode: u64,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let Ok(metadata) = std::fs::symlink_metadata(&self.path) else {
            return;
        };
        if metadata.file_type().is_socket()
            && metadata.uid() == self.owner_uid
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn same_owned_socket(
    original: &std::fs::Metadata,
    current: &std::fs::Metadata,
    owner_uid: u32,
) -> bool {
    original.file_type().is_socket()
        && current.file_type().is_socket()
        && original.uid() == owner_uid
        && current.uid() == owner_uid
        && original.dev() == current.dev()
        && original.ino() == current.ino()
}

pub async fn shutdown_signal() -> std::io::Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::net::UnixListener as StdUnixListener};

    use uuid::Uuid;

    use super::*;
    use crate::effective_uid_from_proc_status;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("gobrowse-socket-{}", Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn current_uid() -> u32 {
        effective_uid_from_proc_status(&fs::read_to_string("/proc/self/status").unwrap()).unwrap()
    }

    fn config(root: &TestDirectory) -> SocketConfig {
        let uid = current_uid();
        SocketConfig {
            path: root.0.join("sandboxd.sock"),
            mode: 0o600,
            owner_uid: uid,
            allowed_peer_uid: uid,
        }
    }

    #[tokio::test]
    async fn owned_stale_socket_is_replaced_and_guard_removes_bound_socket() {
        let root = TestDirectory::new();
        let config = config(&root);
        drop(StdUnixListener::bind(&config.path).unwrap());
        let (listener, guard) = config.bind().await.unwrap();
        assert!(config.path.exists());
        drop(listener);
        drop(guard);
        assert!(!config.path.exists());
    }

    #[tokio::test]
    async fn active_or_insecure_socket_locations_are_rejected() {
        let root = TestDirectory::new();
        let active_config = config(&root);
        let _active = StdUnixListener::bind(&active_config.path).unwrap();
        assert!(matches!(
            active_config.bind().await,
            Err(DaemonError::Conflict)
        ));

        let insecure = TestDirectory::new();
        fs::set_permissions(&insecure.0, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(matches!(
            config(&insecure).bind().await,
            Err(DaemonError::InvalidConfiguration)
        ));
    }

    #[tokio::test]
    async fn socket_mode_and_peer_uid_are_explicit() {
        let root = TestDirectory::new();
        let uid = current_uid();
        let (first, _) = UnixStream::pair().unwrap();
        let same_uid = config(&root);
        assert!(same_uid.validate_peer(&first).is_ok());

        let different_uid = SocketConfig {
            mode: 0o660,
            allowed_peer_uid: uid.saturating_add(1),
            ..same_uid
        };
        assert!(different_uid.validate().is_ok());
        assert!(matches!(
            different_uid.validate_peer(&first),
            Err(DaemonError::Unauthorized)
        ));
        assert!(
            SocketConfig {
                mode: 0o666,
                ..different_uid
            }
            .validate()
            .is_err()
        );
    }
}
