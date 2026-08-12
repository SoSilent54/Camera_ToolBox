//! I²C 进程级设备锁；防止 EEPROM 状态机与 raw I²C transfer 在同一器件上交错。

use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

use fs2::FileExt;

const LOCK_ROOT: &str = "/run/lock";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum I2cAddressMode {
    SevenBit,
    TenBit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct I2cLockKey {
    bus: u32,
    address: u16,
    address_mode: I2cAddressMode,
}

impl I2cLockKey {
    pub(crate) const fn new(bus: u32, address: u16, address_mode: I2cAddressMode) -> Self {
        Self {
            bus,
            address,
            address_mode,
        }
    }

    pub(crate) const fn seven_bit(bus: u32, address: u16) -> Self {
        Self::new(bus, address, I2cAddressMode::SevenBit)
    }

    #[cfg(test)]
    const fn ten_bit(bus: u32, address: u16) -> Self {
        Self::new(bus, address, I2cAddressMode::TenBit)
    }

    fn path(self, root: &Path) -> PathBuf {
        match self.address_mode {
            // 七位地址沿用旧 EEPROM lock 文件名，让新 raw I²C 与既有 EEPROM helper 共享互斥键。
            I2cAddressMode::SevenBit => root.join(format!(
                "camera-toolbox-eeprom-bus-{}-addr-{:02x}.lock",
                self.bus, self.address
            )),
            I2cAddressMode::TenBit => root.join(format!(
                "camera-toolbox-i2c-bus-{}-addr10-{:03x}.lock",
                self.bus, self.address
            )),
        }
    }
}

#[derive(Debug)]
pub(crate) struct I2cDeviceLocks {
    _files: Vec<File>,
}

impl I2cDeviceLocks {
    pub(crate) fn acquire(
        keys: impl IntoIterator<Item = I2cLockKey>,
    ) -> Result<Self, DeviceLockError> {
        Self::acquire_in(Path::new(LOCK_ROOT), keys)
    }

    pub(crate) fn acquire_eeprom(
        i2c_bus: u32,
        device_address: u8,
    ) -> Result<Self, DeviceLockError> {
        Self::acquire_eeprom_in(Path::new(LOCK_ROOT), i2c_bus, device_address)
    }

    fn acquire_eeprom_in(
        root: &Path,
        i2c_bus: u32,
        device_address: u8,
    ) -> Result<Self, DeviceLockError> {
        Self::acquire_in(
            root,
            [I2cLockKey::seven_bit(i2c_bus, u16::from(device_address))],
        )
    }

    fn acquire_in(
        root: &Path,
        keys: impl IntoIterator<Item = I2cLockKey>,
    ) -> Result<Self, DeviceLockError> {
        let mut files = Vec::new();
        for key in keys.into_iter().collect::<BTreeSet<_>>() {
            files.push(acquire_one(root, key)?);
        }
        Ok(Self { _files: files })
    }
}

fn acquire_one(root: &Path, key: I2cLockKey) -> Result<File, DeviceLockError> {
    let path = key.path(root);
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
    Ok(file)
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
                "another I2C helper already holds {}. Do not retry until it exits",
                path.display()
            ),
            Self::Io { path, error } => {
                format!("failed to acquire I2C lock {}: {error}", path.display())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "camera-toolbox-i2c-lock-{label}-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn same_bus_address_and_mode_are_exclusive_until_holder_exits() {
        let root = temp_root("exclusive");

        let first = I2cDeviceLocks::acquire_in(&root, [I2cLockKey::seven_bit(7, 0x50)]).unwrap();
        let second =
            I2cDeviceLocks::acquire_in(&root, [I2cLockKey::seven_bit(7, 0x50)]).unwrap_err();
        assert_eq!(second.code(), "device_busy");

        let other_bus =
            I2cDeviceLocks::acquire_in(&root, [I2cLockKey::seven_bit(8, 0x50)]).unwrap();
        let other_mode = I2cDeviceLocks::acquire_in(&root, [I2cLockKey::ten_bit(7, 0x50)]).unwrap();
        drop(first);
        let replacement =
            I2cDeviceLocks::acquire_in(&root, [I2cLockKey::seven_bit(7, 0x50)]).unwrap();

        drop(replacement);
        drop(other_mode);
        drop(other_bus);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn eeprom_and_raw_i2c_share_the_seven_bit_lock_key() {
        let root = temp_root("eeprom-raw");

        let eeprom = I2cDeviceLocks::acquire_eeprom_in(&root, 7, 0x50).unwrap();
        let raw_i2c =
            I2cDeviceLocks::acquire_in(&root, [I2cLockKey::seven_bit(7, 0x50)]).unwrap_err();

        assert_eq!(raw_i2c.code(), "device_busy");

        drop(eeprom);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn duplicate_keys_in_one_request_are_deduped_before_locking() {
        let root = temp_root("dedup");

        let locks = I2cDeviceLocks::acquire_in(
            &root,
            [
                I2cLockKey::seven_bit(7, 0x50),
                I2cLockKey::seven_bit(7, 0x50),
            ],
        )
        .unwrap();

        drop(locks);
        std::fs::remove_dir_all(root).unwrap();
    }
}
