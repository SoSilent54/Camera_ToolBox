//! Linux i2c-dev 后端；所有页边界和写周期来自固定 storage map。

use std::{thread, time::Duration};

use camera_toolbox_core::CalibrationStorageMap;
use i2cdev::{
    core::{I2CMessage, I2CTransfer},
    linux::{LinuxI2CBus, LinuxI2CMessage},
};

use crate::engine::EepromDevice;

pub(crate) struct LinuxEepromDevice {
    bus: LinuxI2CBus,
    bus_number: u32,
    address: u16,
    page_size: usize,
    write_cycle: Duration,
}

impl LinuxEepromDevice {
    pub(crate) fn open(bus: u32, map: &CalibrationStorageMap) -> Result<Self, String> {
        if map.transport.address_width_bits != 16 {
            return Err(format!(
                "unsupported EEPROM address width {}",
                map.transport.address_width_bits
            ));
        }
        let path = format!("/dev/i2c-{bus}");
        let bus_device = LinuxI2CBus::new(&path).map_err(|error| {
            format!(
                "open {path} address 0x{:02x}: {error}",
                map.transport.i2c_address
            )
        })?;
        Ok(Self {
            bus: bus_device,
            bus_number: bus,
            address: u16::from(map.transport.i2c_address),
            page_size: usize::from(map.transport.page_size_bytes),
            write_cycle: Duration::from_millis(u64::from(map.transport.write_cycle_ms)),
        })
    }

    fn transfer(&mut self, messages: &mut [LinuxI2CMessage<'_>]) -> Result<(), String> {
        self.bus
            .transfer(messages)
            .map(|_| ())
            .map_err(|error| format!("I2C_RDWR on /dev/i2c-{} failed: {error}", self.bus_number))
    }
}

impl EepromDevice for LinuxEepromDevice {
    fn read_range(&mut self, offset: u16, bytes: &mut [u8]) -> Result<(), String> {
        let mut consumed = 0_usize;
        while consumed < bytes.len() {
            let count = self.page_size.min(bytes.len() - consumed);
            let current = usize::from(offset)
                .checked_add(consumed)
                .and_then(|value| u16::try_from(value).ok())
                .ok_or_else(|| "EEPROM read offset overflow".to_owned())?;
            let offset_bytes = current.to_be_bytes();
            let mut read_buffer = vec![0_u8; count];
            let mut messages = [
                LinuxI2CMessage::write(&offset_bytes).with_address(self.address),
                LinuxI2CMessage::read(read_buffer.as_mut_slice()).with_address(self.address),
            ];
            self.transfer(&mut messages).map_err(|error| {
                format!(
                    "set EEPROM read offset 0x{current:04x} via I2C_RDWR on /dev/i2c-{}: {error}",
                    self.bus_number
                )
            })?;
            bytes[consumed..consumed + count].copy_from_slice(&read_buffer);
            consumed += count;
        }
        Ok(())
    }

    fn write_page(&mut self, offset: u16, bytes: &[u8]) -> Result<(), String> {
        if bytes.is_empty() || bytes.len() > self.page_size {
            return Err(format!(
                "EEPROM page payload must contain 1..={} bytes, got {}",
                self.page_size,
                bytes.len()
            ));
        }
        let start = usize::from(offset);
        let page_start = start / self.page_size;
        let page_end = (start + bytes.len() - 1) / self.page_size;
        if page_start != page_end {
            return Err(format!(
                "EEPROM write at 0x{offset:04x} crosses a {}-byte page boundary",
                self.page_size
            ));
        }
        let mut frame = Vec::with_capacity(bytes.len() + 2);
        frame.extend_from_slice(&offset.to_be_bytes());
        frame.extend_from_slice(bytes);
        let mut messages = [LinuxI2CMessage::write(&frame).with_address(self.address)];
        self.transfer(&mut messages).map_err(|error| {
            format!(
                "write EEPROM page at 0x{offset:04x} via I2C_RDWR on /dev/i2c-{}: {error}",
                self.bus_number
            )
        })?;
        thread::sleep(self.write_cycle);
        Ok(())
    }
}
