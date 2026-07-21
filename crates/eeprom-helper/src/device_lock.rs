//! EEPROM 进程级独占锁；防止 SSH 响应丢失后的重复 helper 并发写同一器件。

use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

use fs2::FileExt;

const LOCK_ROOT: &str = "/run/lock";

#[derive(Debug)]
pub(crate) struct EepromDeviceLock {
    _file: File,
}

impl EepromDeviceLock {
    pub(crate) fn acquire(i2c_bus: u32, device_address: u8) -> Result<Self, DeviceLockError> {
        Self::acquire_in(Path::new(LOCK_ROOT), i2c_bus, device_address)
    }

    fn acquire_in(root: &Path, i2c_bus: u32, device_address: u8) -> Result<Self, DeviceLockError> {
        let path = root.join(format!(
            "camera-toolbox-eeprom-bus-{i2c_bus}-addr-{device_address:02x}.lock"
        ));
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| DeviceLockError::Io {
                path: path.clone(),
                error: error.to_string(),
            })?;
        file.try_lock_exclusive().map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                DeviceLockError::Busy(path.clone())
            } else {
                DeviceLockError::Io {
                    path: path.clone(),
                    error: error.to_string(),
                }
            }
        })?;
        Ok(Self { _file: file })
    }
}

#[derive(Debug)]
pub(crate) enum DeviceLockError {
    Busy(PathBuf),
    Io { path: PathBuf, error: String },
}

impl DeviceLockError {
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::Busy(_) => "device_busy",
            Self::Io { .. } => "device_lock_failed",
        }
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::Busy(path) => format!(
                "another EEPROM helper already holds {}. Do not retry until it exits",
                path.display()
            ),
            Self::Io { path, error } => {
                format!("failed to acquire EEPROM lock {}: {error}", path.display())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn same_bus_and_address_are_exclusive_until_holder_exits() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "camera-toolbox-eeprom-lock-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();

        let first = EepromDeviceLock::acquire_in(&root, 7, 0x50).unwrap();
        let second = EepromDeviceLock::acquire_in(&root, 7, 0x50).unwrap_err();
        assert_eq!(second.code(), "device_busy");

        let other_bus = EepromDeviceLock::acquire_in(&root, 8, 0x50).unwrap();
        drop(first);
        let replacement = EepromDeviceLock::acquire_in(&root, 7, 0x50).unwrap();

        drop(replacement);
        drop(other_bus);
        std::fs::remove_dir_all(root).unwrap();
    }
}
