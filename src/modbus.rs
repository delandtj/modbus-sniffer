/// Modbus RTU CRC-16 (CRC-16-IBM, polynomial 0x8005, init 0xFFFF).
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for byte in data {
        crc ^= u16::from(*byte);
        for _ in 0..8 {
            let lsb = crc & 1;
            crc >>= 1;
            if lsb != 0 {
                crc ^= 0xA001;
            }
        }
    }
    crc
}

/// Human-readable name for common Modbus function codes.
pub fn format_function(code: u8) -> &'static str {
    match code {
        0x01 => "Read Coils",
        0x02 => "Read Discrete Inputs",
        0x03 => "Read Holding Registers",
        0x04 => "Read Input Registers",
        0x05 => "Write Single Coil",
        0x06 => "Write Single Register",
        0x07 => "Read Exception Status",
        0x08 => "Diagnostics",
        0x0B => "Get Comm Event Counter",
        0x0C => "Get Comm Event Log",
        0x0F => "Write Multiple Coils",
        0x10 => "Write Multiple Registers",
        0x11 => "Report Slave ID",
        0x14 => "Read File Record",
        0x15 => "Write File Record",
        0x16 => "Mask Write Register",
        0x17 => "Read/Write Multiple Registers",
        0x18 => "Read FIFO Queue",
        0x2B => "Encapsulated Interface Transport",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crc16_known_vector() {
        // Example: slave 01, function 03, read 2 regs from 0000 -> 01 03 00 00 00 02 C4 0B
        let frame = [0x01, 0x03, 0x00, 0x00, 0x00, 0x02];
        assert_eq!(crc16(&frame), 0x0BC4);
    }
}
