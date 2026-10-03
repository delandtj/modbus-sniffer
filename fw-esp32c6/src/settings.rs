//! Persistence of the runtime bus settings.
//!
//! The record lives at offset 0 of the ESP-IDF **`nvs` data partition**, which
//! the espflash default partition table puts at 0x9000 with a size of 0x6000.
//! We do not use IDF's NVS library at all, so that partition's raw space is
//! ours to format; the partition table is read at boot through
//! esp-bootloader-esp-idf's API rather than hard-coding 0x9000, so a custom
//! partition table moves the record along with the partition.
//!
//! Only the first flash sector (4096 bytes) of the partition is touched: a
//! write erases that sector and lays down one 16-byte
//! [`modbus_sniffer_core::encode_bus_record`] record. Anything else (erased
//! flash, a record from a future version, bit rot caught by the CRC) reads back
//! as "no setting" and the firmware falls back to [`sniffer::DEFAULT_BUS`].

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use esp_bootloader_esp_idf::partitions::{
    self, DataPartitionSubType, PartitionEntry, PartitionType,
};
use esp_storage::FlashStorage;

use modbus_sniffer_core as sniffer;
use sniffer::BusConfig;

/// The smallest erasable unit of the flash chip.
const SECTOR: u32 = FlashStorage::SECTOR_SIZE;

struct Store {
    flash: FlashStorage<'static>,
    /// The partition the record lives in, or `None` if the table has no `nvs`
    /// data partition - in which case settings simply do not persist and the
    /// `bus` command says so instead of pretending.
    area: Option<PartitionEntry>,
}

/// Guards the one flash peripheral. An async mutex, because the command
/// handlers that write the record are async and a flash erase takes tens of
/// milliseconds - long enough that spinning would be rude to the sniffer.
static STORE: Mutex<CriticalSectionRawMutex, Option<Store>> = Mutex::new(None);

/// Take ownership of the flash peripheral, locate the settings area and return
/// the stored bus configuration (or the default).
///
/// Call once, from `main`, before anything else touches flash.
pub async fn init(flash: esp_hal::peripherals::FLASH<'static>) -> BusConfig {
    let mut flash = FlashStorage::new(flash);

    // 3 KB on the main task's stack, which is the linker-provided one.
    let mut table = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
    let area = partitions::read_partition_table(&mut flash, &mut table)
        .ok()
        .and_then(|table| {
            table
                .find_partition(PartitionType::Data(DataPartitionSubType::Nvs))
                .ok()
                .flatten()
        })
        .filter(|entry| entry.len() >= SECTOR);

    let mut bus = sniffer::DEFAULT_BUS;
    if let Some(entry) = area {
        let mut raw = [0u8; sniffer::BUS_RECORD_LEN];
        let mut region = entry.as_flash_region(&mut flash);
        if region.read(0, &mut raw).is_ok() {
            if let Some(stored) = sniffer::decode_bus_record(&raw) {
                bus = stored;
            }
        }
    }

    *STORE.lock().await = Some(Store { flash, area });
    bus
}

/// Persist a bus configuration. The `Err` payload is protocol-visible text.
pub async fn store(bus: BusConfig) -> Result<(), &'static str> {
    let mut guard = STORE.lock().await;
    let store = guard.as_mut().ok_or("settings store not initialised")?;
    let entry = store.area.ok_or("no nvs data partition to save into")?;

    let raw = sniffer::encode_bus_record(bus);
    let mut region = entry.as_flash_region(&mut store.flash);
    region.erase(0, SECTOR).map_err(|_| "flash erase failed")?;
    region.write(0, &raw).map_err(|_| "flash write failed")?;
    Ok(())
}
