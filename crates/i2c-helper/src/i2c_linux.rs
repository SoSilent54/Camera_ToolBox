//! Linux `I2C_RDWR` 后端；message 序列按请求顺序映射到一次 ioctl。

use std::{collections::BTreeMap, fs, path::Path};

use camera_toolbox_app::{
    I2cBusInfo, I2cMessageData, I2cMessageFlag, I2cMessageSpec, I2cTransactionResult,
    I2cTransactionSpec,
};
use i2cdev::{
    core::{I2CMessage, I2CTransfer},
    linux::{I2CMessageFlags, LinuxI2CBus, LinuxI2CMessage},
};

use crate::i2c_engine::{I2cBackend, I2cTransferError, result_from_read_buffers};

pub(crate) struct LinuxI2cBackend;

trait LinuxI2cTransfer {
    fn transfer_messages(&mut self, messages: &mut [LinuxI2CMessage<'_>]) -> Result<u32, String>;
}

impl LinuxI2cTransfer for LinuxI2CBus {
    fn transfer_messages(&mut self, messages: &mut [LinuxI2CMessage<'_>]) -> Result<u32, String> {
        self.transfer(messages).map_err(|error| error.to_string())
    }
}

impl I2cBackend for LinuxI2cBackend {
    fn list_buses(&self) -> Result<Vec<I2cBusInfo>, String> {
        list_buses_in(Path::new("/sys/class/i2c-dev"), Path::new("/dev"))
    }

    fn transfer(
        &mut self,
        transaction: &I2cTransactionSpec,
    ) -> Result<I2cTransactionResult, I2cTransferError> {
        let path = format!("/dev/i2c-{}", transaction.bus);
        let mut bus = LinuxI2CBus::new(&path)
            .map_err(|error| I2cTransferError::transaction(format!("open {path}: {error}")))?;
        transfer_on_bus(&mut bus, &path, transaction)
    }
}

fn transfer_on_bus(
    bus: &mut impl LinuxI2cTransfer,
    bus_path: &str,
    transaction: &I2cTransactionSpec,
) -> Result<I2cTransactionResult, I2cTransferError> {
    let mut read_buffers = read_buffers_for(transaction);
    let mut read_iter = read_buffers.iter_mut();
    let mut messages = Vec::with_capacity(transaction.messages.len());

    for message in &transaction.messages {
        let flags = linux_flags_for(message);
        let linux_message = match &message.data {
            I2cMessageData::Write { bytes } => LinuxI2CMessage::write(bytes)
                .with_address(message.address)
                .with_flags(flags),
            I2cMessageData::Read { .. } => {
                let buffer = read_iter
                    .next()
                    .expect("read buffer count matches read message count");
                LinuxI2CMessage::read(buffer.as_mut_slice())
                    .with_address(message.address)
                    .with_flags(flags)
            }
        };
        messages.push(linux_message);
    }

    let transferred_messages = bus.transfer_messages(&mut messages).map_err(|error| {
        I2cTransferError::transaction(format!("I2C_RDWR on {bus_path} failed: {error}"))
    })?;
    let expected_messages =
        u32::try_from(transaction.messages.len()).expect("validated message count fits u32");
    if transferred_messages != expected_messages {
        return Err(I2cTransferError::transaction(format!(
            "I2C_RDWR on {bus_path} transferred {transferred_messages} of {expected_messages} messages; treating partial transfer as failure"
        )));
    }
    drop(messages);

    Ok(result_from_read_buffers(
        transaction,
        transferred_messages,
        &read_buffers,
    ))
}

fn linux_flags_for(message: &I2cMessageSpec) -> I2CMessageFlags {
    let mut flags = match message.data {
        I2cMessageData::Read { .. } => I2CMessageFlags::READ,
        I2cMessageData::Write { .. } => I2CMessageFlags::empty(),
    };
    for flag in &message.flags {
        flags |= match flag {
            I2cMessageFlag::TenBitAddress => I2CMessageFlags::TEN_BIT_ADDRESS,
            I2cMessageFlag::Stop => I2CMessageFlags::STOP,
            I2cMessageFlag::NoStart => I2CMessageFlags::NO_START,
            I2cMessageFlag::IgnoreNack => I2CMessageFlags::IGNORE_NACK,
            I2cMessageFlag::IgnoreAck => I2CMessageFlags::IGNORE_ACK,
        };
    }
    flags
}

fn read_buffers_for(transaction: &I2cTransactionSpec) -> Vec<Vec<u8>> {
    transaction
        .messages
        .iter()
        .filter_map(|message| match message.data {
            I2cMessageData::Write { .. } => None,
            I2cMessageData::Read { byte_len } => Some(vec![0_u8; usize::from(byte_len)]),
        })
        .collect()
}

fn list_buses_in(sysfs_root: &Path, dev_root: &Path) -> Result<Vec<I2cBusInfo>, String> {
    let mut buses = BTreeMap::<u32, I2cBusInfo>::new();
    collect_sysfs_buses(sysfs_root, dev_root, &mut buses)?;
    collect_dev_buses(dev_root, &mut buses)?;
    Ok(buses.into_values().collect())
}

fn collect_sysfs_buses(
    sysfs_root: &Path,
    dev_root: &Path,
    buses: &mut BTreeMap<u32, I2cBusInfo>,
) -> Result<(), String> {
    let entries = match fs::read_dir(sysfs_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("list {}: {error}", sysfs_root.display())),
    };

    for entry in entries {
        let entry =
            entry.map_err(|error| format!("read {} entry: {error}", sysfs_root.display()))?;
        let Some(bus) = parse_i2c_bus_name(&entry.file_name().to_string_lossy()) else {
            continue;
        };
        let dev_path = dev_root.join(format!("i2c-{bus}"));
        buses.insert(
            bus,
            I2cBusInfo {
                bus,
                dev_path: dev_path.display().to_string(),
                name: read_bus_name(&entry.path()),
                dev_node_exists: dev_path.exists(),
            },
        );
    }
    Ok(())
}

fn collect_dev_buses(dev_root: &Path, buses: &mut BTreeMap<u32, I2cBusInfo>) -> Result<(), String> {
    let entries = match fs::read_dir(dev_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("list {}: {error}", dev_root.display())),
    };

    for entry in entries {
        let entry = entry.map_err(|error| format!("read {} entry: {error}", dev_root.display()))?;
        let Some(bus) = parse_i2c_bus_name(&entry.file_name().to_string_lossy()) else {
            continue;
        };
        buses.entry(bus).or_insert_with(|| I2cBusInfo {
            bus,
            dev_path: entry.path().display().to_string(),
            name: None,
            dev_node_exists: true,
        });
    }
    Ok(())
}

fn parse_i2c_bus_name(name: &str) -> Option<u32> {
    name.strip_prefix("i2c-")?.parse().ok()
}

fn read_bus_name(sysfs_bus_path: &Path) -> Option<String> {
    ["name", "device/name"].into_iter().find_map(|relative| {
        let text = fs::read_to_string(sysfs_bus_path.join(relative)).ok()?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    })
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use camera_toolbox_app::I2cMessageDirection;

    fn temp_root() -> std::path::PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "camera-toolbox-i2c-linux-{}-{suffix}",
            std::process::id()
        ))
    }

    fn write_message(address: u16, bytes: &[u8]) -> I2cMessageSpec {
        I2cMessageSpec {
            address,
            flags: Vec::new(),
            data: I2cMessageData::Write {
                bytes: bytes.to_vec(),
            },
        }
    }

    fn read_message(address: u16, byte_len: u16, flags: Vec<I2cMessageFlag>) -> I2cMessageSpec {
        I2cMessageSpec {
            address,
            flags,
            data: I2cMessageData::Read { byte_len },
        }
    }

    #[test]
    fn read_flags_preserve_read_bit_when_optional_flags_are_set() {
        let message = read_message(
            0x50,
            2,
            vec![I2cMessageFlag::IgnoreNack, I2cMessageFlag::Stop],
        );

        let flags = linux_flags_for(&message);

        assert!(flags.contains(I2CMessageFlags::READ));
        assert!(flags.contains(I2CMessageFlags::IGNORE_NACK));
        assert!(flags.contains(I2CMessageFlags::STOP));
    }

    #[test]
    fn write_flags_do_not_inherit_read_bit() {
        let mut message = write_message(0x50, &[0x00]);
        message.flags.push(I2cMessageFlag::IgnoreAck);

        let flags = linux_flags_for(&message);

        assert!(!flags.contains(I2CMessageFlags::READ));
        assert!(flags.contains(I2CMessageFlags::IGNORE_ACK));
    }

    #[test]
    fn read_buffer_backfill_is_reflected_in_transaction_result() {
        let transaction = I2cTransactionSpec {
            bus: 8,
            messages: vec![
                write_message(0x50, &[0x00, 0x10]),
                read_message(0x50, 3, vec![]),
            ],
            settle_ms: None,
        };
        let mut read_buffers = read_buffers_for(&transaction);
        read_buffers[0].copy_from_slice(&[0x11, 0x22, 0x33]);

        let result = result_from_read_buffers(&transaction, 2, &read_buffers);

        assert_eq!(result.messages[0].direction, I2cMessageDirection::Write);
        assert!(result.messages[0].bytes.is_empty());
        assert_eq!(result.messages[1].direction, I2cMessageDirection::Read);
        assert_eq!(result.messages[1].bytes, [0x11, 0x22, 0x33]);
    }

    struct PartialTransferBus {
        transferred_messages: u32,
    }

    impl LinuxI2cTransfer for PartialTransferBus {
        fn transfer_messages(
            &mut self,
            _messages: &mut [LinuxI2CMessage<'_>],
        ) -> Result<u32, String> {
            Ok(self.transferred_messages)
        }
    }

    #[test]
    fn partial_transfer_count_is_failure() {
        let transaction = I2cTransactionSpec {
            bus: 8,
            messages: vec![
                write_message(0x50, &[0x00, 0x10]),
                read_message(0x50, 3, vec![]),
            ],
            settle_ms: None,
        };
        let mut bus = PartialTransferBus {
            transferred_messages: 1,
        };

        let error = transfer_on_bus(&mut bus, "/dev/i2c-8", &transaction).unwrap_err();

        assert!(error.message.contains("transferred 1 of 2 messages"));
    }

    #[test]
    fn list_buses_merges_sysfs_and_dev_nodes() {
        let root = temp_root();
        let sysfs = root.join("sys/class/i2c-dev");
        let dev = root.join("dev");
        fs::create_dir_all(sysfs.join("i2c-7")).unwrap();
        fs::create_dir_all(sysfs.join("i2c-8/device")).unwrap();
        fs::create_dir_all(&dev).unwrap();
        fs::write(sysfs.join("i2c-7/name"), "primary\n").unwrap();
        fs::write(sysfs.join("i2c-8/device/name"), "muxed\n").unwrap();
        fs::write(dev.join("i2c-8"), b"").unwrap();
        fs::write(dev.join("i2c-9"), b"").unwrap();

        let buses = list_buses_in(&sysfs, &dev).unwrap();

        assert_eq!(
            buses.iter().map(|bus| bus.bus).collect::<Vec<_>>(),
            [7, 8, 9]
        );
        assert_eq!(buses[0].name.as_deref(), Some("primary"));
        assert!(!buses[0].dev_node_exists);
        assert_eq!(buses[1].name.as_deref(), Some("muxed"));
        assert!(buses[1].dev_node_exists);
        assert!(buses[2].dev_node_exists);

        fs::remove_dir_all(root).unwrap();
    }
}
