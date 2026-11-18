//! NTFS Boot Sector Parser
//!
//! Parses the NTFS boot sector to extract volume geometry information
//! needed for MFT parsing (bytes per sector, sectors per cluster, MFT location, etc.)

use byteorder::{LittleEndian, ReadBytesExt};
use std::io::{Cursor, Read};
use crate::{VelocityError, VelocityResult};

/// NTFS Boot Sector structure
/// Based on Microsoft's NTFS specification
#[derive(Debug, Clone, Copy)]
pub struct NtfsBootSector {
    pub jump: [u8; 3],
    pub oem_id: [u8; 8],
    pub bytes_per_sector: u16,
    pub sectors_per_cluster: u8,
    pub reserved_sectors: u16,
    pub _unused1: [u8; 3],
    pub _unused2: u16,
    pub media_descriptor: u8,
    pub _unused3: u16,
    pub sectors_per_track: u16,
    pub heads: u16,
    pub hidden_sectors: u32,
    pub _unused4: u32,
    pub _unused5: u32,
    pub total_sectors: u64,
    pub mft_lcn: u64,           // MFT Logical Cluster Number
    pub mft_mirror_lcn: u64,
    pub clusters_per_record: i8,  // negative = 2^abs
    pub _unused6: [u8; 3],
    pub clusters_per_index: i8,
    pub _unused7: [u8; 3],
    pub volume_serial: u64,
    pub checksum: u32,
}

/// Parsed boot sector with derived values
#[derive(Debug, Clone)]
pub struct BootSectorInfo {
    pub bytes_per_sector: u32,
    pub sectors_per_cluster: u32,
    pub bytes_per_cluster: u32,
    pub total_sectors: u64,
    pub mft_lcn: u64,
    pub mft_mirror_lcn: u64,
    pub bytes_per_record: u32,
    pub bytes_per_index_record: u32,
    pub volume_serial: u64,
    pub oem_id: String,
}

impl NtfsBootSector {
    /// Parse NTFS boot sector from raw bytes (512 bytes minimum)
    pub fn parse(data: &[u8]) -> VelocityResult<Self> {
        if data.len() < 512 {
            return Err(VelocityError::Config(format!(
                "Boot sector data too small: {} bytes (need at least 512)",
                data.len()
            )));
        }

        let mut cursor = Cursor::new(data);

        // Read jump instruction (3 bytes)
        let mut jump = [0u8; 3];
        cursor.read_exact(&mut jump)?;

        // Read OEM ID (8 bytes)
        let mut oem_id = [0u8; 8];
        cursor.read_exact(&mut oem_id)?;

        // Verify OEM ID is "NTFS    "
        if &oem_id != b"NTFS    " {
            return Err(VelocityError::Config(format!(
                "Invalid OEM ID: {:?} (expected 'NTFS    ')",
                String::from_utf8_lossy(&oem_id)
            )));
        }

        let bytes_per_sector = cursor.read_u16::<LittleEndian>()?;
        let sectors_per_cluster = cursor.read_u8()?;
        let reserved_sectors = cursor.read_u16::<LittleEndian>()?;
        let mut _unused1 = [0u8; 3];
        cursor.read_exact(&mut _unused1)?;
        let _unused2 = cursor.read_u16::<LittleEndian>()?;
        let media_descriptor = cursor.read_u8()?;
        let _unused3 = cursor.read_u16::<LittleEndian>()?;
        let sectors_per_track = cursor.read_u16::<LittleEndian>()?;
        let heads = cursor.read_u16::<LittleEndian>()?;
        let hidden_sectors = cursor.read_u32::<LittleEndian>()?;
        let _unused4 = cursor.read_u32::<LittleEndian>()?;
        let _unused5 = cursor.read_u32::<LittleEndian>()?;
        let total_sectors = cursor.read_u64::<LittleEndian>()?;
        let mft_lcn = cursor.read_u64::<LittleEndian>()?;
        let mft_mirror_lcn = cursor.read_u64::<LittleEndian>()?;
        let clusters_per_record = cursor.read_i8()?;
        let mut _unused6 = [0u8; 3];
        cursor.read_exact(&mut _unused6)?;
        let clusters_per_index = cursor.read_i8()?;
        let mut _unused7 = [0u8; 3];
        cursor.read_exact(&mut _unused7)?;
        let volume_serial = cursor.read_u64::<LittleEndian>()?;
        let checksum = cursor.read_u32::<LittleEndian>()?;

        let cluster_bytes = u32::from(bytes_per_sector) * u32::from(sectors_per_cluster);
        let record_bytes = if clusters_per_record > 0 {
            u32::from(clusters_per_record as u8).checked_mul(cluster_bytes)
        } else {
            1u32.checked_shl((-(clusters_per_record as i16)) as u32)
        };
        if !matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096)
            || !sectors_per_cluster.is_power_of_two()
            || sectors_per_cluster > 128
            || clusters_per_record == 0 || clusters_per_record < -31
            || clusters_per_index == 0 || clusters_per_index < -31
            || !matches!(record_bytes, Some(512..=1_048_576)) {
            return Err(VelocityError::Config("Invalid NTFS volume geometry".to_string()));
        }

        Ok(Self {
            jump,
            oem_id,
            bytes_per_sector,
            sectors_per_cluster,
            reserved_sectors,
            _unused1,
            _unused2,
            media_descriptor,
            _unused3,
            sectors_per_track,
            heads,
            hidden_sectors,
            _unused4,
            _unused5,
            total_sectors,
            mft_lcn,
            mft_mirror_lcn,
            clusters_per_record,
            _unused6,
            clusters_per_index,
            _unused7,
            volume_serial,
            checksum,
        })
    }

    /// Convert to BootSectorInfo with derived values
    pub fn to_info(&self) -> BootSectorInfo {
        let bytes_per_sector = self.bytes_per_sector as u32;
        let sectors_per_cluster = self.sectors_per_cluster as u32;
        let bytes_per_cluster = bytes_per_sector * sectors_per_cluster;

        // Calculate bytes per MFT record
        let bytes_per_record = if self.clusters_per_record > 0 {
            self.clusters_per_record as u32 * bytes_per_cluster
        } else {
            // Negative value means 2^abs(clusters_per_record)
            1u32.checked_shl((-(self.clusters_per_record as i16)) as u32).unwrap_or(0)
        };

        // Calculate bytes per index record
        let bytes_per_index_record = if self.clusters_per_index > 0 {
            self.clusters_per_index as u32 * bytes_per_cluster
        } else {
            1u32.checked_shl((-(self.clusters_per_index as i16)) as u32).unwrap_or(0)
        };

        // Convert OEM ID to string
        let oem_id = String::from_utf8_lossy(&self.oem_id).trim_end().to_string();

        BootSectorInfo {
            bytes_per_sector,
            sectors_per_cluster,
            bytes_per_cluster,
            total_sectors: self.total_sectors,
            mft_lcn: self.mft_lcn,
            mft_mirror_lcn: self.mft_mirror_lcn,
            bytes_per_record,
            bytes_per_index_record,
            volume_serial: self.volume_serial,
            oem_id,
        }
    }

    /// Get bytes per cluster
    pub fn bytes_per_cluster(&self) -> u32 {
        self.bytes_per_sector as u32 * self.sectors_per_cluster as u32
    }

    /// Get bytes per MFT record
    pub fn bytes_per_record(&self) -> u32 {
        if self.clusters_per_record > 0 {
            self.clusters_per_record as u32 * self.bytes_per_cluster()
        } else {
            1u32.checked_shl((-(self.clusters_per_record as i16)) as u32).unwrap_or(0)
        }
    }
}

/// Read boot sector from a volume handle
pub fn read_boot_sector(volume_path: &str) -> VelocityResult<BootSectorInfo> {
    use std::fs::File;
    use std::io::Read;

    let mut file = File::open(volume_path)
        .map_err(|e| VelocityError::Config(format!("Failed to open volume '{}': {}", volume_path, e)))?;

    let mut buffer = [0u8; 512];
    file.read_exact(&mut buffer)
        .map_err(|e| VelocityError::Config(format!("Failed to read boot sector: {}", e)))?;

    let boot_sector = NtfsBootSector::parse(&buffer)?;
    Ok(boot_sector.to_info())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid_ntfs_boot_sector() {
        // Create a minimal valid NTFS boot sector for testing
        let mut data = vec![0u8; 512];

        // Jump instruction (JMP SHORT + NOP)
        data[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);

        // OEM ID: "NTFS    "
        data[3..11].copy_from_slice(b"NTFS    ");

        // Bytes per sector: 512 (0x0200)
        data[11..13].copy_from_slice(&0x0200u16.to_le_bytes());

        // Sectors per cluster: 8
        data[13] = 8;

        // Reserved sectors: 0
        data[14..16].copy_from_slice(&0u16.to_le_bytes());

        // ... (rest zeros for unused fields)

        // Total sectors: some value
        data[40..48].copy_from_slice(&1000000u64.to_le_bytes());

        // MFT LCN: 4 (typical)
        data[48..56].copy_from_slice(&4u64.to_le_bytes());

        // MFT Mirror LCN: 500000
        data[56..64].copy_from_slice(&500000u64.to_le_bytes());

        // Clusters per file record: -10 (1024 bytes)
        data[64] = 0xF6; // -10 as i8

        // Clusters per index record: 1
        data[68] = 1;

        // Volume serial
        data[72..80].copy_from_slice(&0x123456789ABCDEFFu64.to_le_bytes());

        let boot_sector = NtfsBootSector::parse(&data).expect("Failed to parse test boot sector");
        let info = boot_sector.to_info();

        assert_eq!(info.bytes_per_sector, 512);
        assert_eq!(info.sectors_per_cluster, 8);
        assert_eq!(info.bytes_per_cluster, 4096);
        assert_eq!(info.mft_lcn, 4);
        assert_eq!(info.mft_mirror_lcn, 500000);
        assert_eq!(info.bytes_per_record, 1024); // 2^10
        assert_eq!(info.bytes_per_index_record, 4096); // 1 * 4096
        assert_eq!(info.volume_serial, 0x123456789ABCDEFF);
        assert_eq!(info.oem_id, "NTFS");
    }

    #[test]
    fn test_rejects_invalid_geometry_without_panicking() {
        let mut data = vec![0u8; 512];
        data[3..11].copy_from_slice(b"NTFS    ");
        data[11..13].copy_from_slice(&512u16.to_le_bytes());
        data[13] = 8;
        data[64] = 0x80; // i8::MIN cannot represent a valid record size
        data[68] = 1;
        assert!(NtfsBootSector::parse(&data).is_err());
        data[64] = 0xf6;
        data[13] = 0;
        assert!(NtfsBootSector::parse(&data).is_err());
    }

    #[test]
    fn test_invalid_oem_id() {
        let mut data = vec![0u8; 512];
        data[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
        data[3..11].copy_from_slice(b"FAT32   "); // Wrong OEM ID
        data[11..13].copy_from_slice(&0x0200u16.to_le_bytes());
        data[13] = 8;

        let result = NtfsBootSector::parse(&data);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid OEM ID"));
    }

    #[test]
    fn test_too_small_data() {
        let data = vec![0u8; 100];
        let result = NtfsBootSector::parse(&data);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too small"));
    }
}