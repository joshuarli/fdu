use crate::indexed::BulkMetadata;
use fdu_core::{EntryType, FileIdentity};
use std::ffi::CStr;
use std::io;

const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;
const SF_FIRMLINK: u32 = 0x0080_0000;
const STAT_BLOCK_BYTES: u64 = 512;

const COMMON_ATTRIBUTES: libc::attrgroup_t = libc::ATTR_CMN_RETURNED_ATTRS
    | libc::ATTR_CMN_NAME
    | libc::ATTR_CMN_DEVID
    | libc::ATTR_CMN_OBJTYPE
    | libc::ATTR_CMN_FLAGS
    | libc::ATTR_CMN_FILEID;
const FILE_ATTRIBUTES: libc::attrgroup_t =
    libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE | libc::ATTR_FILE_DATALENGTH;

#[repr(align(8))]
pub(crate) struct AlignedBuffer<const BYTES: usize>([u8; BYTES]);

impl<const BYTES: usize> AlignedBuffer<BYTES> {
    pub(crate) fn new() -> Self {
        Self([0; BYTES])
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub(crate) fn as_mut_bytes(&mut self) -> &mut [u8] {
        &mut self.0
    }
}

#[derive(Debug)]
pub(super) struct BulkEntry<'a> {
    pub(super) name: &'a CStr,
    pub(super) metadata: Option<BulkMetadata>,
}

pub(super) fn requested_attributes() -> libc::attrlist {
    libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: COMMON_ATTRIBUTES,
        volattr: 0,
        dirattr: 0,
        fileattr: FILE_ATTRIBUTES,
        forkattr: 0,
    }
}

pub(super) fn read_record_length(bytes: &[u8]) -> io::Result<usize> {
    let mut cursor = Cursor::new(bytes);
    usize::try_from(cursor.take_u32()?)
        .map_err(|_| invalid_data("macOS attribute record length exceeds the address space"))
}

pub(super) fn parse_record(bytes: &[u8]) -> io::Result<BulkEntry<'_>> {
    if bytes.len() < 24 {
        return Err(invalid_data("macOS attribute record is truncated"));
    }

    let mut cursor = Cursor::new(bytes);
    let record_length = usize::try_from(cursor.take_u32()?)
        .map_err(|_| invalid_data("macOS attribute record length exceeds the address space"))?;
    if record_length != bytes.len() {
        return Err(invalid_data("macOS attribute record length does not match its buffer"));
    }
    let returned_common = cursor.take_u32()?;
    let _returned_volume = cursor.take_u32()?;
    let _returned_directory = cursor.take_u32()?;
    let returned_file = cursor.take_u32()?;
    let _returned_fork = cursor.take_u32()?;
    if returned_common & libc::ATTR_CMN_RETURNED_ATTRS == 0 {
        return Err(invalid_data("macOS attributes omit their returned bitmap"));
    }

    let reference_offset = cursor.position;
    let name_offset = cursor.take_i32()?;
    let name_length = usize::try_from(cursor.take_u32()?)
        .map_err(|_| invalid_data("macOS filename exceeds the address space"))?;
    let name = if returned_common & libc::ATTR_CMN_NAME != 0 {
        let start = reference_offset
            .checked_add_signed(isize::try_from(name_offset).map_err(|_| {
                invalid_data("macOS filename has an invalid offset")
            })?)
            .ok_or_else(|| invalid_data("macOS filename has an invalid offset"))?;
        let end = start
            .checked_add(name_length)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| invalid_data("macOS filename exceeds its record"))?;
        let filename = CStr::from_bytes_with_nul(&bytes[start..end])
            .map_err(|_| invalid_data("macOS directory entry has an invalid name"))?;
        Some(filename)
    } else {
        None
    };

    // FSOPT_PACK_INVAL_ATTRS keeps the requested fields packed even when the returned bitmap
    // marks one unavailable, so the parser can advance through a fixed common-attribute layout.
    let raw_device = cursor.take_i32()?;
    let raw_entry_type = cursor.take_u32()?;
    let flags = cursor.take_u32()?;
    let inode = cursor.take_u64()?;
    let entry_type = entry_type_for_vnode(raw_entry_type);

    let mut raw_link_count = 0;
    let mut raw_allocated_bytes = 0;
    let mut raw_apparent_bytes = 0;
    if entry_type != EntryType::Directory {
        raw_link_count = cursor.take_u32()?;
        raw_allocated_bytes = cursor.take_i64()?;
        raw_apparent_bytes = cursor.take_i64()?;
    }

    let required_common = libc::ATTR_CMN_NAME
        | libc::ATTR_CMN_DEVID
        | libc::ATTR_CMN_OBJTYPE
        | libc::ATTR_CMN_FLAGS
        | libc::ATTR_CMN_FILEID;
    let metadata = if returned_common & required_common == required_common && flags & SF_FIRMLINK == 0 {
        let device = u64::try_from(i64::from(raw_device)).ok();
        match (device, entry_type) {
            (Some(device), EntryType::Directory) => Some(BulkMetadata {
                identity: FileIdentity { device, inode },
                entry_type,
                link_count: 0,
                apparent_bytes: 0,
                allocated_bytes: 0,
            }),
            (Some(device), _) if returned_file & FILE_ATTRIBUTES == FILE_ATTRIBUTES => {
                let apparent_bytes = u64::try_from(raw_apparent_bytes).ok();
                let allocated_bytes = u64::try_from(raw_allocated_bytes)
                    .ok()
                    .and_then(round_allocated_bytes);
                match (apparent_bytes, allocated_bytes) {
                    (Some(apparent_bytes), Some(allocated_bytes)) => Some(BulkMetadata {
                        identity: FileIdentity { device, inode },
                        entry_type,
                        link_count: u64::from(raw_link_count),
                        apparent_bytes,
                        allocated_bytes,
                    }),
                    _ => None,
                }
            }
            _ => None,
        }
    } else {
        None
    };

    Ok(BulkEntry {
        name: name.ok_or_else(|| invalid_data("macOS directory record has no filename"))?,
        metadata,
    })
}

fn entry_type_for_vnode(vnode_type: u32) -> EntryType {
    match vnode_type {
        VDIR => EntryType::Directory,
        VREG => EntryType::RegularFile,
        VLNK => EntryType::Symlink,
        _ => EntryType::Other,
    }
}

fn round_allocated_bytes(bytes: u64) -> Option<u64> {
    bytes.div_ceil(STAT_BLOCK_BYTES).checked_mul(STAT_BLOCK_BYTES)
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let end = self
            .position
            .checked_add(N)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| invalid_data("macOS attribute record is truncated"))?;
        let value = self.bytes[self.position..end]
            .try_into()
            .map_err(|_| invalid_data("macOS attribute record is truncated"))?;
        self.position = end;
        Ok(value)
    }

    fn take_u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_ne_bytes(self.take()?))
    }

    fn take_i32(&mut self) -> io::Result<i32> {
        Ok(i32::from_ne_bytes(self.take()?))
    }

    fn take_u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_ne_bytes(self.take()?))
    }

    fn take_i64(&mut self) -> io::Result<i64> {
        Ok(i64::from_ne_bytes(self.take()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMON: u32 = libc::ATTR_CMN_RETURNED_ATTRS
        | libc::ATTR_CMN_NAME
        | libc::ATTR_CMN_DEVID
        | libc::ATTR_CMN_OBJTYPE
        | libc::ATTR_CMN_FLAGS
        | libc::ATTR_CMN_FILEID;

    fn file_record(returned_common: u32, returned_file: u32, flags: u32) -> Vec<u8> {
        let mut bytes = vec![0; 80];
        put_u32(&mut bytes, 0, 80);
        put_u32(&mut bytes, 4, returned_common);
        put_u32(&mut bytes, 16, returned_file);
        put_i32(&mut bytes, 24, 48);
        put_u32(&mut bytes, 28, 6);
        put_i32(&mut bytes, 32, 17);
        put_u32(&mut bytes, 36, VREG);
        put_u32(&mut bytes, 40, flags);
        put_u64(&mut bytes, 44, 29);
        put_u32(&mut bytes, 52, 2);
        put_i64(&mut bytes, 56, 513);
        put_i64(&mut bytes, 64, 91);
        bytes[72..78].copy_from_slice(b"entry\0");
        bytes
    }

    fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
    }

    fn put_i32(bytes: &mut [u8], offset: usize, value: i32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
    }

    fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
    }

    fn put_i64(bytes: &mut [u8], offset: usize, value: i64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
    }

    #[test]
    fn parses_bulk_file_metadata_and_rounds_allocations_to_stat_blocks() {
        let bytes = file_record(COMMON, FILE_ATTRIBUTES, 0);
        let entry = parse_record(&bytes).unwrap();

        assert_eq!(entry.name.to_bytes(), b"entry");
        assert_eq!(
            entry.metadata,
            Some(BulkMetadata {
                identity: FileIdentity {
                    device: 17,
                    inode: 29,
                },
                entry_type: EntryType::RegularFile,
                link_count: 2,
                apparent_bytes: 91,
                allocated_bytes: 1024,
            })
        );
    }

    #[test]
    fn parses_directory_identity_without_using_bulk_link_count() {
        let mut bytes = file_record(COMMON, 0, 0);
        put_u32(&mut bytes, 36, VDIR);
        let entry = parse_record(&bytes).unwrap();

        assert_eq!(
            entry.metadata,
            Some(BulkMetadata {
                identity: FileIdentity {
                    device: 17,
                    inode: 29,
                },
                entry_type: EntryType::Directory,
                link_count: 0,
                apparent_bytes: 0,
                allocated_bytes: 0,
            })
        );
    }

    #[test]
    fn keeps_names_when_bulk_metadata_is_unavailable_or_a_firmlink() {
        let missing_flags = COMMON & !libc::ATTR_CMN_FLAGS;
        let missing_bytes = file_record(missing_flags, FILE_ATTRIBUTES, 0);
        let firmlink_bytes = file_record(COMMON, FILE_ATTRIBUTES, SF_FIRMLINK);
        let missing = parse_record(&missing_bytes).unwrap();
        let firmlink = parse_record(&firmlink_bytes).unwrap();

        assert_eq!(missing.name.to_bytes(), b"entry");
        assert!(missing.metadata.is_none());
        assert_eq!(firmlink.name.to_bytes(), b"entry");
        assert!(firmlink.metadata.is_none());
    }

    #[test]
    fn rejects_filename_references_outside_the_record() {
        let mut bytes = file_record(COMMON, FILE_ATTRIBUTES, 0);
        put_i32(&mut bytes, 24, i32::MAX);

        assert_eq!(parse_record(&bytes).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
