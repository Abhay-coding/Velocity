//! MFT Record Parser
//!
//! Parses NTFS MFT FILE records and their attributes.
//! Handles both resident and non-resident attributes.

use byteorder::{LittleEndian, ReadBytesExt};
use std::io::{Cursor, Read};
use crate::{VelocityError, VelocityResult, boot_sector::BootSectorInfo};

/// NTFS Attribute Type IDs
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributeType {
    StandardInformation = 0x10,
    AttributeList = 0x20,
    FileName = 0x30,
    ObjectId = 0x40,
    SecurityDescriptor = 0x50,
    VolumeName = 0x60,
    VolumeInformation = 0x70,
    Data = 0x80,
    IndexRoot = 0x90,
    IndexAllocation = 0xA0,
    Bitmap = 0xB0,
    ReparsePoint = 0xC0,
    EaInformation = 0xD0,
    Ea = 0xE0,
    PropertySet = 0xF0,
    LoggedUtilityStream = 0x100,
    EndMarker = 0xFFFFFFFF,
}

impl AttributeType {
    pub fn from_u32(val: u32) -> Option<Self> {
        match val {
            0x10 => Some(Self::StandardInformation),
            0x20 => Some(Self::AttributeList),
            0x30 => Some(Self::FileName),
            0x40 => Some(Self::ObjectId),
            0x50 => Some(Self::SecurityDescriptor),
            0x60 => Some(Self::VolumeName),
            0x70 => Some(Self::VolumeInformation),
            0x80 => Some(Self::Data),
            0x90 => Some(Self::IndexRoot),
            0xA0 => Some(Self::IndexAllocation),
            0xB0 => Some(Self::Bitmap),
            0xC0 => Some(Self::ReparsePoint),
            0xD0 => Some(Self::EaInformation),
            0xE0 => Some(Self::Ea),
            0xF0 => Some(Self::PropertySet),
            0x100 => Some(Self::LoggedUtilityStream),
            0xFFFFFFFF => Some(Self::EndMarker),
            _ => None,
        }
    }

    pub fn as_u32(&self) -> u32 {
        *self as u32
    }
}

/// FILE_RECORD_HEADER structure
#[derive(Debug, Clone, Copy)]
pub struct FileRecordHeader {
    pub signature: [u8; 4],           // "FILE"
    pub usa_offset: u16,              // Offset to Update Sequence Array
    pub usa_count: u16,               // Number of entries in USA (including sequence number)
    pub lsn: u64,                     // Log Sequence Number
    pub sequence_number: u16,         // Sequence number
    pub hard_link_count: u16,         // Hard link count
    pub first_attribute_offset: u16,  // Offset to first attribute
    pub flags: u16,                   // Flags (1=in_use, 2=directory)
    pub used_size: u32,               // Real size of the record
    pub allocated_size: u32,          // Allocated size of the record
    pub base_record: u64,             // Base file record (0 if base)
    pub next_attribute_id: u16,       // Next attribute ID
    pub _padding: u16,                // Padding to align to 8 bytes
}

impl FileRecordHeader {
    /// Parse FILE record header from raw bytes
    pub fn parse(data: &[u8]) -> VelocityResult<Self> {
        if data.len() < 48 {
            return Err(VelocityError::Config(format!(
                "FILE record too small: {} bytes (need at least 48)",
                data.len()
            )));
        }

        let mut cursor = Cursor::new(data);

        let mut signature = [0u8; 4];
        cursor.read_exact(&mut signature)?;

        // Verify "FILE" signature
        if &signature != b"FILE" {
            return Err(VelocityError::Config(format!(
                "Invalid FILE record signature: {:?} (expected 'FILE')",
                String::from_utf8_lossy(&signature)
            )));
        }

        let usa_offset = cursor.read_u16::<LittleEndian>()?;
        let usa_count = cursor.read_u16::<LittleEndian>()?;
        let lsn = cursor.read_u64::<LittleEndian>()?;
        let sequence_number = cursor.read_u16::<LittleEndian>()?;
        let hard_link_count = cursor.read_u16::<LittleEndian>()?;
        let first_attribute_offset = cursor.read_u16::<LittleEndian>()?;
        let flags = cursor.read_u16::<LittleEndian>()?;
        let used_size = cursor.read_u32::<LittleEndian>()?;
        let allocated_size = cursor.read_u32::<LittleEndian>()?;
        let base_record = cursor.read_u64::<LittleEndian>()?;
        let next_attribute_id = cursor.read_u16::<LittleEndian>()?;
        let _padding = cursor.read_u16::<LittleEndian>()?;

        Ok(Self {
            signature,
            usa_offset,
            usa_count,
            lsn,
            sequence_number,
            hard_link_count,
            first_attribute_offset,
            flags,
            used_size,
            allocated_size,
            base_record,
            next_attribute_id,
            _padding,
        })
    }

    /// Check if record is in use
    pub fn is_in_use(&self) -> bool {
        self.flags & 0x01 != 0
    }

    /// Check if record is a directory
    pub fn is_directory(&self) -> bool {
        self.flags & 0x02 != 0
    }

    /// Get file reference number (combines record number and sequence number)
    pub fn file_reference(&self, record_number: u64) -> u64 {
        (record_number & 0x0000_ffff_ffff_ffff) | ((self.sequence_number as u64) << 48)
    }
}

/// Attribute header (common for resident and non-resident)
#[derive(Debug, Clone, Copy)]
pub struct AttributeHeader {
    pub type_id: u32,
    pub length: u32,
    pub non_resident: u8,
    pub name_length: u8,
    pub name_offset: u16,
    pub flags: u16,
    pub attribute_id: u16,
}

impl AttributeHeader {
    pub fn parse(data: &[u8]) -> VelocityResult<Self> {
        if data.len() < 24 {
            return Err(VelocityError::Config(format!(
                "Attribute header too small: {} bytes (need at least 24)",
                data.len()
            )));
        }

        let mut cursor = Cursor::new(data);
        let type_id = cursor.read_u32::<LittleEndian>()?;
        let length = cursor.read_u32::<LittleEndian>()?;
        let non_resident = cursor.read_u8()?;
        let name_length = cursor.read_u8()?;
        let name_offset = cursor.read_u16::<LittleEndian>()?;
        let flags = cursor.read_u16::<LittleEndian>()?;
        let attribute_id = cursor.read_u16::<LittleEndian>()?;

        // Verify it's not the end marker
        if type_id == 0xFFFFFFFF {
            return Err(VelocityError::Config("End marker attribute".to_string()));
        }

        // Basic validation
        if length < 24 || length > data.len() as u32 {
            return Err(VelocityError::Config(format!(
                "Invalid attribute length: {} (data len: {})",
                length, data.len()
            )));
        }

        Ok(Self {
            type_id,
            length,
            non_resident,
            name_length,
            name_offset,
            flags,
            attribute_id,
        })
    }

    pub fn is_resident(&self) -> bool {
        self.non_resident == 0
    }

    pub fn type_id(&self) -> Option<AttributeType> {
        AttributeType::from_u32(self.type_id)
    }
}

/// Resident attribute header (extends common header)
#[derive(Debug, Clone, Copy)]
pub struct ResidentAttributeHeader {
    pub common: AttributeHeader,
    pub value_length: u32,
    pub value_offset: u16,
    pub indexed_flag: u8,
    pub _padding: u8,
}

impl ResidentAttributeHeader {
    pub fn parse(data: &[u8]) -> VelocityResult<Self> {
        let common = AttributeHeader::parse(data)?;
        if !common.is_resident() {
            return Err(VelocityError::Config("Not a resident attribute".to_string()));
        }
        if data.len() < 24 {
            return Err(VelocityError::Config("Resident attribute header too small".to_string()));
        }

        let mut cursor = Cursor::new(&data[16..]);
        let value_length = cursor.read_u32::<LittleEndian>()?;
        let value_offset = cursor.read_u16::<LittleEndian>()?;
        let indexed_flag = cursor.read_u8()?;
        let _padding = cursor.read_u8()?;

        Ok(Self {
            common,
            value_length,
            value_offset,
            indexed_flag,
            _padding,
        })
    }

    /// Get the value data slice
    pub fn value<'a>(&self, data: &'a [u8]) -> VelocityResult<&'a [u8]> {
        let start = self.value_offset as usize;
        let end = start + self.value_length as usize;
        if end > data.len() {
            return Err(VelocityError::Config("Value extends beyond attribute data".to_string()));
        }
        Ok(&data[start..end])
    }
}

/// Non-resident attribute header (extends common header)
#[derive(Debug, Clone, Copy)]
pub struct NonResidentAttributeHeader {
    pub common: AttributeHeader,
    pub start_vcn: u64,
    pub last_vcn: u64,
    pub data_run_offset: u16,
    pub compression_unit: u16,
    pub _unused: u32,
    pub allocated_size: u64,
    pub real_size: u64,
    pub initialized_size: u64,
}

impl NonResidentAttributeHeader {
    pub fn parse(data: &[u8]) -> VelocityResult<Self> {
        let common = AttributeHeader::parse(data)?;
        if common.is_resident() {
            return Err(VelocityError::Config("Not a non-resident attribute".to_string()));
        }
        if data.len() < 64 {
            return Err(VelocityError::Config("Non-resident attribute header too small".to_string()));
        }

        let mut cursor = Cursor::new(&data[16..]);
        let start_vcn = cursor.read_u64::<LittleEndian>()?;
        let last_vcn = cursor.read_u64::<LittleEndian>()?;
        let data_run_offset = cursor.read_u16::<LittleEndian>()?;
        let compression_unit = cursor.read_u16::<LittleEndian>()?;
        let _unused = cursor.read_u32::<LittleEndian>()?;
        let allocated_size = cursor.read_u64::<LittleEndian>()?;
        let real_size = cursor.read_u64::<LittleEndian>()?;
        let initialized_size = cursor.read_u64::<LittleEndian>()?;

        Ok(Self {
            common,
            start_vcn,
            last_vcn,
            data_run_offset,
            compression_unit,
            _unused,
            allocated_size,
            real_size,
            initialized_size,
        })
    }
}

/// Parsed attribute enum
#[derive(Debug, Clone)]
pub enum MftAttribute {
    StandardInformation(StandardInformation),
    FileName(FileNameAttribute),
    Data(DataAttribute),
    Other {
        type_id: u32,
        attribute_id: u16,
        name: Option<String>,
        resident: bool,
        data: Vec<u8>,
    },
}

impl MftAttribute {
    pub fn type_id(&self) -> u32 {
        match self {
            Self::StandardInformation(_) => 0x10,
            Self::FileName(_) => 0x30,
            Self::Data(_) => 0x80,
            Self::Other { type_id, .. } => *type_id,
        }
    }

    pub fn attribute_id(&self) -> u16 {
        match self {
            Self::StandardInformation(si) => si.attribute_id,
            Self::FileName(fn_attr) => fn_attr.attribute_id,
            Self::Data(d) => d.attribute_id,
            Self::Other { attribute_id, .. } => *attribute_id,
        }
    }

    pub fn name(&self) -> Option<&str> {
        match self {
            Self::StandardInformation(_) => None,
            Self::FileName(fn_attr) => Some(&fn_attr.name),
            Self::Data(d) => d.name.as_deref(),
            Self::Other { name, .. } => name.as_deref(),
        }
    }
}

/// $STANDARD_INFORMATION attribute (0x10)
#[derive(Debug, Clone)]
pub struct StandardInformation {
    pub attribute_id: u16,
    pub creation_time: u64,      // Windows FILETIME (100ns since 1601-01-01)
    pub modification_time: u64,
    pub mft_modification_time: u64,
    pub access_time: u64,
    pub file_attributes: u32,
    pub max_versions: u32,
    pub version_number: u32,
    pub class_id: u32,
    pub owner_id: u32,
    pub security_id: u32,
    pub quota_charged: u64,
    pub usn: u64,
}

impl StandardInformation {
    pub fn parse(data: &[u8], attribute_id: u16) -> VelocityResult<Self> {
        if data.len() < 48 {
            return Err(VelocityError::Config(format!(
                "StandardInformation too small: {} bytes (need at least 48)",
                data.len()
            )));
        }

        let mut cursor = Cursor::new(data);
        let creation_time = cursor.read_u64::<LittleEndian>()?;
        let modification_time = cursor.read_u64::<LittleEndian>()?;
        let mft_modification_time = cursor.read_u64::<LittleEndian>()?;
        let access_time = cursor.read_u64::<LittleEndian>()?;
        let file_attributes = cursor.read_u32::<LittleEndian>()?;
        let max_versions = cursor.read_u32::<LittleEndian>()?;
        let version_number = cursor.read_u32::<LittleEndian>()?;
        let class_id = cursor.read_u32::<LittleEndian>()?;
        let owner_id = cursor.read_u32::<LittleEndian>()?;
        let security_id = cursor.read_u32::<LittleEndian>()?;
        let quota_charged = cursor.read_u64::<LittleEndian>()?;
        let usn = cursor.read_u64::<LittleEndian>()?;

        Ok(Self {
            attribute_id,
            creation_time,
            modification_time,
            mft_modification_time,
            access_time,
            file_attributes,
            max_versions,
            version_number,
            class_id,
            owner_id,
            security_id,
            quota_charged,
            usn,
        })
    }
}

/// $FILE_NAME attribute (0x30)
#[derive(Debug, Clone)]
pub struct FileNameAttribute {
    pub attribute_id: u16,
    pub parent_file_reference: u64,
    pub creation_time: u64,
    pub modification_time: u64,
    pub mft_modification_time: u64,
    pub access_time: u64,
    pub allocated_size: u64,
    pub real_size: u64,
    pub file_attributes: u32,
    pub name_length: u8,
    pub namespace: u8,  // 0=POSIX, 1=Win32, 2=DOS, 3=Win32&DOS
    pub name: String,
}

impl FileNameAttribute {
    pub fn parse(data: &[u8], attribute_id: u16) -> VelocityResult<Self> {
        if data.len() < 66 {
            return Err(VelocityError::Config(format!(
                "FileNameAttribute too small: {} bytes (need at least 66)",
                data.len()
            )));
        }

        let mut cursor = Cursor::new(data);
        let parent_file_reference = cursor.read_u64::<LittleEndian>()?;
        let creation_time = cursor.read_u64::<LittleEndian>()?;
        let modification_time = cursor.read_u64::<LittleEndian>()?;
        let mft_modification_time = cursor.read_u64::<LittleEndian>()?;
        let access_time = cursor.read_u64::<LittleEndian>()?;
        let allocated_size = cursor.read_u64::<LittleEndian>()?;
        let real_size = cursor.read_u64::<LittleEndian>()?;
        let file_attributes = cursor.read_u32::<LittleEndian>()?;
        let _packed_ea_reparse = cursor.read_u32::<LittleEndian>()?; // Packed EA size / Reparse point tag
        let name_length = cursor.read_u8()?;
        let namespace = cursor.read_u8()?;

        // Name is UTF-16LE
        let name_bytes_needed = name_length as usize * 2;
        let remaining = data.len() - 66;
        if remaining < name_bytes_needed {
            return Err(VelocityError::Config(format!(
                "FileNameAttribute name data too small: {} bytes (need {})",
                remaining, name_bytes_needed
            )));
        }

        let name_data = &data[66..66 + name_bytes_needed];
        let name = String::from_utf16(
            &name_data
                .chunks_exact(2)
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                .collect::<Vec<u16>>()
        ).map_err(|e| VelocityError::Config(format!("Invalid UTF-16 in filename: {}", e)))?;

        Ok(Self {
            attribute_id,
            parent_file_reference,
            creation_time,
            modification_time,
            mft_modification_time,
            access_time,
            allocated_size,
            real_size,
            file_attributes,
            name_length,
            namespace,
            name,
        })
    }
}

/// $DATA attribute (0x80)
#[derive(Debug, Clone)]
pub struct DataAttribute {
    pub attribute_id: u16,
    pub name: Option<String>,
    pub resident: bool,
    pub data: Vec<u8>,           // For resident
    pub runlist: Vec<RunlistEntry>, // For non-resident
    pub allocated_size: u64,
    pub real_size: u64,
    pub initialized_size: u64,
}

impl DataAttribute {
    pub fn parse_resident(data: &[u8], attribute_id: u16, name: Option<String>) -> VelocityResult<Self> {
        Ok(Self {
            attribute_id,
            name,
            resident: true,
            data: data.to_vec(),
            runlist: Vec::new(),
            allocated_size: data.len() as u64,
            real_size: data.len() as u64,
            initialized_size: data.len() as u64,
        })
    }

    pub fn parse_non_resident(
        header: &NonResidentAttributeHeader,
        data: &[u8],
        boot_sector: &BootSectorInfo,
        attribute_id: u16,
        name: Option<String>,
    ) -> VelocityResult<Self> {
        let runlist_offset = header.data_run_offset as usize;
        if runlist_offset < 64 || runlist_offset >= data.len() {
            return Err(VelocityError::Config("Runlist offset out of bounds".to_string()));
        }
        let runlist_data = &data[runlist_offset..];
        let runlist = parse_runlist(runlist_data, boot_sector)?;

        Ok(Self {
            attribute_id,
            name,
            resident: false,
            data: Vec::new(),
            runlist,
            allocated_size: header.allocated_size,
            real_size: header.real_size,
            initialized_size: header.initialized_size,
        })
    }
}

/// Runlist entry for non-resident data
#[derive(Debug, Clone)]
pub struct RunlistEntry {
    pub length: u64,      // Length in clusters
    pub lcn: i64,         // Logical Cluster Number (can be -1 for sparse)
    pub next_vcn: u64,    // Next VCN (Virtual Cluster Number)
}

/// Parse NTFS runlist (variable-length encoding)
pub fn parse_runlist(data: &[u8], _boot_sector: &BootSectorInfo) -> VelocityResult<Vec<RunlistEntry>> {
    let mut entries = Vec::new();
    let mut offset = 0usize;
    let mut current_lcn: i64 = 0;
    let mut current_vcn: u64 = 0;

    while offset < data.len() {
        let header = data[offset];
        offset += 1;
        if header == 0 { return Ok(entries); }
        let length_bytes = (header & 0x0F) as usize;
        let offset_bytes = (header >> 4) as usize;
        if !(1..=8).contains(&length_bytes) || offset_bytes > 8 {
            return Err(VelocityError::Config("Invalid runlist field width".to_string()));
        }
        if offset + length_bytes + offset_bytes > data.len() {
            return Err(VelocityError::Config("Runlist truncated".to_string()));
        }
        let mut length = 0u64;
        for i in 0..length_bytes { length |= (data[offset + i] as u64) << (i * 8); }
        offset += length_bytes;
        if length == 0 { return Err(VelocityError::Config("Zero-length run".to_string())); }
        let lcn = if offset_bytes == 0 {
            -1 // Sparse runs do not change the previous physical LCN.
        } else {
            let mut bytes = [if data[offset + offset_bytes - 1] & 0x80 != 0 { 0xff } else { 0 }; 8];
            bytes[..offset_bytes].copy_from_slice(&data[offset..offset + offset_bytes]);
            let delta = i64::from_le_bytes(bytes);
            current_lcn = current_lcn.checked_add(delta)
                .ok_or_else(|| VelocityError::Config("Runlist LCN overflow".to_string()))?;
            if current_lcn < 0 { return Err(VelocityError::Config("Negative runlist LCN".to_string())); }
            current_lcn
        };
        offset += offset_bytes;
        current_vcn = current_vcn.checked_add(length)
            .ok_or_else(|| VelocityError::Config("Runlist VCN overflow".to_string()))?;
        entries.push(RunlistEntry { length, lcn, next_vcn: current_vcn });
    }
    Err(VelocityError::Config("Runlist missing terminator".to_string()))
}

/// Complete MFT record with all attributes
#[derive(Debug, Clone)]
pub struct MftRecord {
    pub record_number: u64,
    pub header: FileRecordHeader,
    pub attributes: Vec<MftAttribute>,
    pub file_name: Option<String>,
    pub parent_ref: Option<u64>,
    pub is_directory: bool,
    pub size: u64,
    pub creation_time: u64,
    pub modification_time: u64,
    pub file_attributes: u32,
}

impl MftRecord {
    /// Parse a complete MFT record from raw bytes
    pub fn parse(data: &[u8], record_number: u64, boot_sector: &BootSectorInfo) -> VelocityResult<Self> {
        let header = FileRecordHeader::parse(data)?;

        // Validate record size
        if header.used_size as usize > data.len() {
            return Err(VelocityError::Config(format!(
                "FILE record used_size {} exceeds data length {}",
                header.used_size, data.len()
            )));
        }

        // Apply USA (Update Sequence Array) fixup if present
        let fixed_data = apply_usa_fixup(data, &header)?;

        // Parse attributes
        let mut attributes = Vec::new();
        let mut attr_offset = header.first_attribute_offset as usize;

        let used_size = header.used_size as usize;
        if attr_offset < 48 || attr_offset > used_size {
            return Err(VelocityError::Config("Invalid first attribute offset".to_string()));
        }
        let mut found_end = false;
        while attr_offset < used_size {
            if attr_offset + 4 > used_size {
                return Err(VelocityError::Config("Truncated attribute end marker".to_string()));
            }

            // Check for end marker
            let type_id = u32::from_le_bytes([
                fixed_data[attr_offset],
                fixed_data[attr_offset + 1],
                fixed_data[attr_offset + 2],
                fixed_data[attr_offset + 3],
            ]);

            if type_id == 0xFFFFFFFF {
                found_end = true;
                break;
            }

            // Parse attribute header to get length
            if attr_offset + 24 > used_size {
                return Err(VelocityError::Config("Truncated attribute header".to_string()));
            }
            let length = u32::from_le_bytes([
                fixed_data[attr_offset + 4],
                fixed_data[attr_offset + 5],
                fixed_data[attr_offset + 6],
                fixed_data[attr_offset + 7],
            ]) as usize;

            if length < 24 || length > used_size - attr_offset {
                return Err(VelocityError::Config("Invalid attribute length".to_string()));
            }

            let attr_data = &fixed_data[attr_offset..attr_offset + length];
            let attr_header = AttributeHeader::parse(attr_data)?;

            // Parse attribute based on type
            let attribute = parse_attribute(attr_data, &attr_header, boot_sector)?;
            attributes.push(attribute);

            attr_offset += length;
        }

        if !found_end {
            return Err(VelocityError::Config("Missing attribute end marker".to_string()));
        }

        // Extract key information from attributes
        let mut file_name = None;
        let mut parent_ref = None;
        let mut size = 0u64;
        let mut creation_time = 0u64;
        let mut modification_time = 0u64;
        let mut file_attributes = 0u32;

        for attr in &attributes {
            match attr {
                MftAttribute::FileName(fn_attr) => {
                    // Prefer Win32 namespace (1) or Win32&DOS (3)
                    if fn_attr.namespace == 1 || fn_attr.namespace == 3 {
                        file_name = Some(fn_attr.name.clone());
                        parent_ref = Some(fn_attr.parent_file_reference);
                        file_attributes = fn_attr.file_attributes;
                        creation_time = fn_attr.creation_time;
                        modification_time = fn_attr.modification_time;
                        size = fn_attr.real_size;
                    } else if file_name.is_none() {
                        // Fallback to first name
                        file_name = Some(fn_attr.name.clone());
                        parent_ref = Some(fn_attr.parent_file_reference);
                        file_attributes = fn_attr.file_attributes;
                        creation_time = fn_attr.creation_time;
                        modification_time = fn_attr.modification_time;
                        size = fn_attr.real_size;
                    }
                }
                MftAttribute::StandardInformation(si) => {
                    if creation_time == 0 {
                        creation_time = si.creation_time;
                        modification_time = si.modification_time;
                        file_attributes = si.file_attributes;
                    }
                }
                MftAttribute::Data(d) => {
                    if !d.resident && size == 0 {
                        size = d.real_size;
                    }
                }
                _ => {}
            }
        }

        let is_directory = header.is_directory();

        Ok(Self {
            record_number,
            header,
            attributes,
            file_name,
            parent_ref,
            is_directory,
            size,
            creation_time,
            modification_time,
            file_attributes,
        })
    }
}

/// Apply USA (Update Sequence Array) fixup to correct sector data
fn apply_usa_fixup(data: &[u8], header: &FileRecordHeader) -> VelocityResult<Vec<u8>> {
    const STRIDE: usize = 512;
    let usa_offset = header.usa_offset as usize;
    let usa_count = header.usa_count as usize;
    if data.len() % STRIDE != 0 || usa_count != data.len() / STRIDE + 1
        || usa_offset < 8 || usa_offset.checked_add(usa_count * 2).map_or(true, |end| end > data.len()) {
        return Err(VelocityError::Config("Invalid update sequence array".to_string()));
    }
    let sequence = [data[usa_offset], data[usa_offset + 1]];
    let mut fixed = data.to_vec();
    for sector in 0..usa_count - 1 {
        let trailer = (sector + 1) * STRIDE - 2;
        if data[trailer..trailer + 2] != sequence {
            return Err(VelocityError::Config("Update sequence mismatch".to_string()));
        }
        let replacement = usa_offset + (sector + 1) * 2;
        fixed[trailer..trailer + 2].copy_from_slice(&data[replacement..replacement + 2]);
    }
    Ok(fixed)
}

/// Parse an attribute based on its header
fn parse_attribute(
    data: &[u8],
    header: &AttributeHeader,
    boot_sector: &BootSectorInfo,
) -> VelocityResult<MftAttribute> {
    let attr_type = header.type_id();
    let attribute_id = header.attribute_id;

    // Read attribute name if present
    let name = if header.name_length > 0 && header.name_offset > 0 {
        let name_start = header.name_offset as usize;
        let name_len = header.name_length as usize * 2; // UTF-16
        if name_start + name_len <= data.len() {
            let name_data = &data[name_start..name_start + name_len];
            String::from_utf16(
                &name_data
                    .chunks_exact(2)
                    .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                    .collect::<Vec<u16>>()
            ).ok()
        } else {
            None
        }
    } else {
        None
    };

    match attr_type {
        Some(AttributeType::StandardInformation) => {
            let resident_header = ResidentAttributeHeader::parse(data)?;
            let value = resident_header.value(data)?;
            let si = StandardInformation::parse(value, attribute_id)?;
            Ok(MftAttribute::StandardInformation(si))
        }
        Some(AttributeType::FileName) => {
            let resident_header = ResidentAttributeHeader::parse(data)?;
            let value = resident_header.value(data)?;
            let fn_attr = FileNameAttribute::parse(value, attribute_id)?;
            Ok(MftAttribute::FileName(fn_attr))
        }
        Some(AttributeType::Data) => {
            if header.is_resident() {
                let resident_header = ResidentAttributeHeader::parse(data)?;
                let value = resident_header.value(data)?;
                let data_attr = DataAttribute::parse_resident(value, attribute_id, name)?;
                Ok(MftAttribute::Data(data_attr))
            } else {
                let non_resident_header = NonResidentAttributeHeader::parse(data)?;
                let data_attr = DataAttribute::parse_non_resident(
                    &non_resident_header,
                    data,
                    boot_sector,
                    attribute_id,
                    name,
                )?;
                Ok(MftAttribute::Data(data_attr))
            }
        }
        _ => {
            // Other attribute types - store raw data
            Ok(MftAttribute::Other {
                type_id: header.type_id,
                attribute_id,
                name,
                resident: header.is_resident(),
                data: data.to_vec(),
            })
        }
    }
}

/// Convert FILETIME (100ns since 1601-01-01) to Unix timestamp (seconds since 1970-01-01)
pub fn filetime_to_unix(filetime: u64) -> u64 {
    if filetime == 0 {
        return 0;
    }
    // Windows FILETIME epoch: 1601-01-01
    // Unix epoch: 1970-01-01
    // Difference: 11644473600 seconds = 116444736000000000 100ns intervals
    const EPOCH_DIFF: u64 = 116444736000000000;
    if filetime <= EPOCH_DIFF {
        return 0;
    }
    (filetime - EPOCH_DIFF) / 10_000_000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry() -> BootSectorInfo {
        BootSectorInfo { bytes_per_sector: 512, sectors_per_cluster: 8, bytes_per_cluster: 4096,
            total_sectors: 100000, mft_lcn: 4, mft_mirror_lcn: 50, bytes_per_record: 1024,
            bytes_per_index_record: 4096, volume_serial: 0, oem_id: "NTFS".into() }
    }

    fn protected_record() -> Vec<u8> {
        let mut data = vec![0u8; 1024];
        data[0..4].copy_from_slice(b"FILE");
        data[4..6].copy_from_slice(&48u16.to_le_bytes());
        data[6..8].copy_from_slice(&3u16.to_le_bytes());
        data[16..18].copy_from_slice(&9u16.to_le_bytes());
        data[20..22].copy_from_slice(&64u16.to_le_bytes());
        data[22..24].copy_from_slice(&1u16.to_le_bytes());
        data[24..28].copy_from_slice(&100u32.to_le_bytes());
        data[28..32].copy_from_slice(&1024u32.to_le_bytes());
        data[48..54].copy_from_slice(&[0xaa, 0xbb, 0x12, 0x34, 0x56, 0x78]);
        data[510..512].copy_from_slice(&[0xaa, 0xbb]);
        data[1022..1024].copy_from_slice(&[0xaa, 0xbb]);
        // A correctly positioned resident $DATA attribute.
        data[64..68].copy_from_slice(&0x80u32.to_le_bytes());
        data[68..72].copy_from_slice(&32u32.to_le_bytes());
        data[80..84].copy_from_slice(&4u32.to_le_bytes());
        data[84..86].copy_from_slice(&24u16.to_le_bytes());
        data[88..92].copy_from_slice(b"test");
        data[96..100].copy_from_slice(&0xffffffffu32.to_le_bytes());
        data
    }

    #[test]
    fn file_reference_uses_low_48_bits_and_high_sequence() {
        let data = protected_record();
        let header = FileRecordHeader::parse(&data).unwrap();
        assert_eq!(header.file_reference(0x1234_5678_9abc), 0x0009_1234_5678_9abc);
    }

    #[test]
    fn fixup_restores_both_sector_trailers_and_parses_resident_data() {
        let data = protected_record();
        let header = FileRecordHeader::parse(&data).unwrap();
        let fixed = apply_usa_fixup(&data, &header).unwrap();
        assert_eq!(&fixed[510..512], &[0x12, 0x34]);
        assert_eq!(&fixed[1022..1024], &[0x56, 0x78]);
        let parsed = MftRecord::parse(&data, 42, &geometry()).unwrap();
        match &parsed.attributes[0] { MftAttribute::Data(d) => assert_eq!(d.data, b"test"), _ => panic!("expected data") }
    }

    #[test]
    fn fixup_rejects_torn_sector_and_invalid_count() {
        let mut data = protected_record();
        data[1022] = 0;
        assert!(MftRecord::parse(&data, 42, &geometry()).is_err());
        let mut data = protected_record();
        data[6..8].copy_from_slice(&2u16.to_le_bytes());
        assert!(MftRecord::parse(&data, 42, &geometry()).is_err());
    }

    #[test]
    fn rejects_bad_attributes_and_missing_end_marker() {
        let mut data = protected_record();
        data[68..72].copy_from_slice(&0u32.to_le_bytes());
        assert!(MftRecord::parse(&data, 42, &geometry()).is_err());
        let mut data = protected_record();
        data[96..100].fill(0);
        assert!(MftRecord::parse(&data, 42, &geometry()).is_err());
        let mut data = protected_record();
        data[24..28].copy_from_slice(&90u32.to_le_bytes());
        assert!(MftRecord::parse(&data, 42, &geometry()).is_err());
    }

    #[test]
    fn sparse_runs_preserve_previous_lcn_for_relative_offsets() {
        let runs = parse_runlist(&[0x11, 2, 100, 0x01, 3, 0x11, 1, 0xfe, 0], &geometry()).unwrap();
        assert_eq!(runs.iter().map(|r| r.lcn).collect::<Vec<_>>(), [100, -1, 98]);
        assert_eq!(runs[2].next_vcn, 6);
    }

    #[test]
    fn rejects_malformed_and_overflowing_runlists() {
        for data in [&[0x19, 1, 2, 3, 4, 5, 6, 7, 8, 9, 1, 0][..],
            &[0x11, 1][..], &[0x11, 0, 1, 0][..], &[0x11, 1, 0xff, 0][..],
            &[0x11, 1, 1][..]] {
            assert!(parse_runlist(data, &geometry()).is_err(), "accepted {:?}", data);
        }
        // Eight-byte signed offset must not overflow during sign extension.
        let runs = parse_runlist(&[0x81, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0], &geometry()).unwrap();
        assert_eq!(runs[0].lcn, 1);
    }

    #[test]
    fn rejects_nonresident_runlist_outside_attribute() {
        let mut data = vec![0u8; 64];
        data[0..4].copy_from_slice(&0x80u32.to_le_bytes());
        data[4..8].copy_from_slice(&64u32.to_le_bytes());
        data[8] = 1;
        data[32..34].copy_from_slice(&1024u16.to_le_bytes());
        let header = NonResidentAttributeHeader::parse(&data).unwrap();
        assert!(DataAttribute::parse_non_resident(&header, &data, &geometry(), 0, None).is_err());
    }

    #[test]
    fn test_filerecord_header_parse() {
        let mut data = vec![0u8; 512];
        data[0..4].copy_from_slice(b"FILE");
        data[4..6].copy_from_slice(&48u16.to_le_bytes()); // usa_offset
        data[6..8].copy_from_slice(&2u16.to_le_bytes());  // usa_count
        data[20..22].copy_from_slice(&48u16.to_le_bytes()); // first_attribute_offset
        data[22..24].copy_from_slice(&1u16.to_le_bytes());  // flags (in_use)

        let header = FileRecordHeader::parse(&data).expect("Failed to parse header");
        assert_eq!(&header.signature, b"FILE");
        assert_eq!(header.usa_offset, 48);
        assert_eq!(header.usa_count, 2);
        assert!(header.is_in_use());
        assert!(!header.is_directory());
    }

    #[test]
    fn test_filerecord_header_invalid_signature() {
        let mut data = vec![0u8; 512];
        data[0..4].copy_from_slice(b"BAAD"); // Invalid signature

        let result = FileRecordHeader::parse(&data);
        assert!(result.is_err());
    }

    #[test]
    fn test_attribute_header_parse() {
        let mut data = vec![0u8; 100];
        data[0..4].copy_from_slice(&0x30u32.to_le_bytes()); // type_id = $FILE_NAME
        data[4..8].copy_from_slice(&96u32.to_le_bytes());   // length
        data[8] = 0; // resident
        data[9] = 0; // name_length
        data[10..12].copy_from_slice(&0u16.to_le_bytes());  // name_offset
        data[12..14].copy_from_slice(&0u16.to_le_bytes());  // flags
        data[14..16].copy_from_slice(&1u16.to_le_bytes());  // attribute_id

        let header = AttributeHeader::parse(&data).expect("Failed to parse attribute header");
        assert_eq!(header.type_id, 0x30);
        assert_eq!(header.length, 96);
        assert!(header.is_resident());
        assert_eq!(header.attribute_id, 1);
        assert_eq!(header.type_id(), Some(AttributeType::FileName));
    }

    #[test]
    fn test_filetime_to_unix() {
        // 1601-01-01 should be 0
        assert_eq!(filetime_to_unix(0), 0);

        // 1970-01-01 should be 0
        const EPOCH_DIFF: u64 = 116444736000000000;
        assert_eq!(filetime_to_unix(EPOCH_DIFF), 0);

        // 1970-01-02 should be 86400
        assert_eq!(filetime_to_unix(EPOCH_DIFF + 86400 * 10_000_000), 86400);
    }

    #[test]
    fn test_parse_runlist() {
        let boot_sector = BootSectorInfo {
            bytes_per_sector: 512,
            sectors_per_cluster: 8,
            bytes_per_cluster: 4096,
            total_sectors: 1000000,
            mft_lcn: 4,
            mft_mirror_lcn: 500000,
            bytes_per_record: 1024,
            bytes_per_index_record: 4096,
            volume_serial: 0,
            oem_id: "NTFS".to_string(),
        };

        // Simple runlist: length=10 clusters, lcn=100; length=5 clusters, lcn=200; end
        // Header byte: length_bytes=1, offset_bytes=1 => 0x11
        // Length: 10 (0x0A)
        // Offset: 100 (0x64)
        // Header byte: length_bytes=1, offset_bytes=1 => 0x11
        // Length: 5 (0x05)
        // Offset: 100 (0x64) relative to previous = 200
        // End: 0x00
        let runlist_data = vec![
            0x11, 0x0A, 0x64,  // First run: length=10, lcn=100
            0x11, 0x05, 0x64,  // Second run: length=5, lcn=100 (relative) = 200
            0x00,              // End
        ];

        let entries = parse_runlist(&runlist_data, &boot_sector).expect("Failed to parse runlist");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].length, 10);
        assert_eq!(entries[0].lcn, 100);
        assert_eq!(entries[1].length, 5);
        assert_eq!(entries[1].lcn, 200);
    }
}
