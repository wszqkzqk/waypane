use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use crate::error::Error;

pub const DEFAULT_MAX_SHM_FILE_SIZE: usize = 512 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rid(pub i32);

#[derive(Debug)]
struct ShmFile {
    data: Vec<u8>,
    damage: Vec<Range<usize>>,
}

impl ShmFile {
    fn record_damage(&mut self, range: Range<usize>) {
        if let Some(last) = self.damage.last_mut() {
            if range.start <= last.end {
                last.end = last.end.max(range.end);
                return;
            }
        }
        self.damage.push(range);
    }
}

#[derive(Debug)]
pub struct ShmStore {
    files: BTreeMap<Rid, ShmFile>,
    fresh: BTreeSet<Rid>,
    max_file_size: usize,
}

impl ShmStore {
    pub fn new(max_file_size: usize) -> Self {
        Self {
            files: BTreeMap::new(),
            fresh: BTreeSet::new(),
            max_file_size,
        }
    }

    pub fn open_file(&mut self, rid: Rid, size: i32) -> Result<usize, Error> {
        let size = self.validate_size(size)?;
        if self.files.contains_key(&rid) || self.fresh.contains(&rid) {
            return Err(Error::DuplicateRid(rid));
        }
        self.files.insert(
            rid,
            ShmFile {
                data: vec![0; size],
                damage: Vec::new(),
            },
        );
        self.fresh.insert(rid);
        Ok(size)
    }

    pub fn extend_file(&mut self, rid: Rid, size: i32) -> Result<usize, Error> {
        let size = self.validate_size(size)?;
        let file = self.files.get_mut(&rid).ok_or(Error::UnknownRid(rid))?;
        if size < file.data.len() {
            return Err(Error::InvalidFrame(format!(
                "ExtendFile would shrink RID {} from {} to {size} bytes",
                rid.0,
                file.data.len()
            )));
        }
        file.data.resize(size, 0);
        Ok(size)
    }

    pub fn inject(&mut self, rid: Rid) -> Result<(), Error> {
        if !self.files.contains_key(&rid) {
            return Err(Error::UnknownRid(rid));
        }
        if !self.fresh.remove(&rid) {
            return Err(Error::InvalidFrame(format!(
                "RID {} was injected before creation or more than once",
                rid.0
            )));
        }
        Ok(())
    }

    #[allow(clippy::single_range_in_vec_init)]
    pub fn apply_fill(&mut self, rid: Rid, payload: &[u8]) -> Result<Vec<Range<usize>>, Error> {
        if payload.len() < 8 {
            return Err(Error::InvalidFrame(
                "BufferFill payload is shorter than 8 bytes".to_string(),
            ));
        }
        let start = u32::from_le_bytes(payload[..4].try_into().unwrap()) as usize;
        let end = u32::from_le_bytes(payload[4..8].try_into().unwrap()) as usize;
        let bytes = &payload[8..];
        let expected = end
            .checked_sub(start)
            .ok_or_else(|| Error::InvalidFrame(format!("invalid fill range {start}..{end}")))?;
        if bytes.len() != expected {
            return Err(Error::InvalidFrame(format!(
                "BufferFill range is {expected} bytes but payload has {}",
                bytes.len()
            )));
        }

        let file = self.files.get_mut(&rid).ok_or(Error::UnknownRid(rid))?;
        let file_size = file.data.len();
        let destination = file.data.get_mut(start..end).ok_or_else(|| {
            Error::InvalidFrame(format!(
                "BufferFill range {start}..{end} exceeds file size {file_size}"
            ))
        })?;
        destination.copy_from_slice(bytes);
        if start < end {
            file.record_damage(start..end);
            Ok(vec![start..end])
        } else {
            Ok(Vec::new())
        }
    }

    pub fn apply_diff(&mut self, rid: Rid, payload: &[u8]) -> Result<Vec<Range<usize>>, Error> {
        if payload.len() < 8 {
            return Err(Error::InvalidFrame(
                "BufferDiff payload is shorter than 8 bytes".to_string(),
            ));
        }
        let diff_size = u32::from_le_bytes(payload[..4].try_into().unwrap()) as usize;
        let trailing_size = u32::from_le_bytes(payload[4..8].try_into().unwrap()) as usize;
        if !diff_size.is_multiple_of(4) {
            return Err(Error::InvalidFrame(format!(
                "diff length {diff_size} is not divisible by 4"
            )));
        }
        let expected = diff_size
            .checked_add(trailing_size)
            .and_then(|size| size.checked_add(8))
            .ok_or_else(|| Error::InvalidFrame("diff size overflow".to_string()))?;
        if payload.len() != expected {
            return Err(Error::InvalidFrame(format!(
                "BufferDiff payload has {} bytes, expected {expected}",
                payload.len()
            )));
        }

        let file = self.files.get_mut(&rid).ok_or(Error::UnknownRid(rid))?;
        if trailing_size > file.data.len() {
            return Err(Error::InvalidFrame(format!(
                "diff trailing size {trailing_size} exceeds file size {}",
                file.data.len()
            )));
        }

        let diff = &payload[8..8 + diff_size];
        let trailing = &payload[8 + diff_size..];
        let file_size = file.data.len();
        let mut changed = Vec::new();
        let mut offset = 0;

        while offset < diff.len() {
            if diff.len() - offset < 8 {
                return Err(Error::InvalidFrame(
                    "incomplete diff interval header".to_string(),
                ));
            }
            let span_start =
                u32::from_le_bytes(diff[offset..offset + 4].try_into().unwrap()) as usize;
            let span_end =
                u32::from_le_bytes(diff[offset + 4..offset + 8].try_into().unwrap()) as usize;
            let words = span_end
                .checked_sub(span_start)
                .filter(|words| *words > 0)
                .ok_or_else(|| {
                    Error::InvalidFrame(format!("invalid diff span {span_start}..{span_end}"))
                })?;
            let copy_size = words
                .checked_mul(4)
                .ok_or_else(|| Error::InvalidFrame("diff span overflow".to_string()))?;
            if diff.len() - offset - 8 < copy_size {
                return Err(Error::InvalidFrame(
                    "diff interval exceeds diff payload".to_string(),
                ));
            }

            let byte_start = span_start
                .checked_mul(4)
                .ok_or_else(|| Error::InvalidFrame("diff offset overflow".to_string()))?;
            let byte_end = span_end
                .checked_mul(4)
                .ok_or_else(|| Error::InvalidFrame("diff offset overflow".to_string()))?;
            if byte_end > file_size {
                return Err(Error::InvalidFrame(format!(
                    "diff range {byte_start}..{byte_end} exceeds file size {file_size}"
                )));
            }

            let data_start = offset + 8;
            file.data[byte_start..byte_end]
                .copy_from_slice(&diff[data_start..data_start + copy_size]);
            file.record_damage(byte_start..byte_end);
            changed.push(byte_start..byte_end);
            offset = data_start + copy_size;
        }

        if trailing_size > 0 {
            let start = file_size - trailing_size;
            file.data[start..].copy_from_slice(trailing);
            file.record_damage(start..file_size);
            changed.push(start..file_size);
        }
        Ok(changed)
    }

    pub fn data(&self, rid: Rid) -> Option<&[u8]> {
        self.files.get(&rid).map(|file| file.data.as_slice())
    }

    pub fn damage(&self, rid: Rid) -> Option<&[Range<usize>]> {
        self.files.get(&rid).map(|file| file.damage.as_slice())
    }

    pub fn take_damage(&mut self, rid: Rid) -> Option<Vec<Range<usize>>> {
        self.files
            .get_mut(&rid)
            .map(|file| std::mem::take(&mut file.damage))
    }

    pub fn contains(&self, rid: Rid) -> bool {
        self.files.contains_key(&rid)
    }

    fn validate_size(&self, size: i32) -> Result<usize, Error> {
        let size = usize::try_from(size)
            .map_err(|_| Error::InvalidFrame(format!("negative shared-memory size {size}")))?;
        if size > self.max_file_size {
            return Err(Error::InvalidFrame(format!(
                "shared-memory size {size} exceeds limit {}",
                self.max_file_size
            )));
        }
        Ok(size)
    }
}
