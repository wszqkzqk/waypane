use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::Range;
use std::sync::Arc;

use crate::compression::decompress;
use crate::error::Error;
use crate::header::Compression;
use crate::wire::MessageType;

pub const DEFAULT_MAX_SHM_FILE_SIZE: usize = 256 * 1024 * 1024;
pub const DEFAULT_MAX_TOTAL_SHM_SIZE: usize = 1024 * 1024 * 1024;
pub const DEFAULT_MAX_PIPE_BUFFER_SIZE: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rid(pub i32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeOpenKind {
    IR,
    IW,
    RW,
}

impl PipeOpenKind {
    pub fn message_type(self) -> MessageType {
        match self {
            Self::IR => MessageType::OpenIrPipe,
            Self::IW => MessageType::OpenIwPipe,
            Self::RW => MessageType::OpenRwPipe,
        }
    }

    fn local_access(self, opened_locally: bool) -> PipeAccess {
        let can_read = matches!(
            (self, opened_locally),
            (Self::IW, false) | (Self::IR, true) | (Self::RW, _)
        );
        let can_write = matches!(
            (self, opened_locally),
            (Self::IR, false) | (Self::IW, true) | (Self::RW, _)
        );
        PipeAccess {
            can_read,
            can_write,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipeAccess {
    pub can_read: bool,
    pub can_write: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeRead {
    pub data: Vec<u8>,
    pub eof: bool,
}

#[derive(Debug)]
struct RemoteFile {
    data: Vec<u8>,
    damage: Vec<Range<usize>>,
}

impl RemoteFile {
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
struct LocalFile {
    data: Arc<[u8]>,
    export_started: bool,
}

#[derive(Debug)]
struct Pipe {
    access: PipeAccess,
    incoming: VecDeque<u8>,
    read_eof: bool,
    remote_read_closed: bool,
    local_read_closed: bool,
    local_write_closed: bool,
    pending_open: Option<PipeOpenKind>,
}

#[derive(Debug)]
pub struct ResourceStore {
    remote_files: BTreeMap<Rid, RemoteFile>,
    local_files: BTreeMap<Rid, LocalFile>,
    pipes: BTreeMap<Rid, Pipe>,
    fresh: BTreeSet<Rid>,
    max_file_size: usize,
    max_transfer_size: usize,
    max_total_file_size: usize,
    max_pipe_buffer_size: usize,
    allocated_file_size: usize,
}

impl ResourceStore {
    pub fn new(max_file_size: usize, max_transfer_size: usize, max_total_file_size: usize) -> Self {
        Self {
            remote_files: BTreeMap::new(),
            local_files: BTreeMap::new(),
            pipes: BTreeMap::new(),
            fresh: BTreeSet::new(),
            max_file_size,
            max_transfer_size,
            max_total_file_size,
            max_pipe_buffer_size: DEFAULT_MAX_PIPE_BUFFER_SIZE,
            allocated_file_size: 0,
        }
    }

    pub fn open_file(&mut self, rid: Rid, size: i32) -> Result<usize, Error> {
        let size = self.validate_new_allocation(size, 0)?;
        self.ensure_new_rid(rid)?;
        self.remote_files.insert(
            rid,
            RemoteFile {
                data: vec![0; size],
                damage: Vec::new(),
            },
        );
        self.fresh.insert(rid);
        self.allocated_file_size += size;
        Ok(size)
    }

    pub fn create_local_readonly_file(&mut self, rid: Rid, data: Arc<[u8]>) -> Result<(), Error> {
        let size = i32::try_from(data.len())
            .map_err(|_| Error::InvalidFrame("local file is too large for Waypipe".to_string()))?;
        self.validate_new_allocation(size, 0)?;
        self.ensure_new_rid(rid)?;
        self.allocated_file_size += data.len();
        self.local_files.insert(
            rid,
            LocalFile {
                data,
                export_started: false,
            },
        );
        self.fresh.insert(rid);
        Ok(())
    }

    pub fn extend_file(&mut self, rid: Rid, size: i32) -> Result<usize, Error> {
        let requested = self.validate_size(size)?;
        let old_size = self
            .remote_files
            .get(&rid)
            .ok_or(Error::UnknownRid(rid))?
            .data
            .len();
        if requested < old_size {
            return Err(Error::InvalidFrame(format!(
                "ExtendFile would shrink RID {} from {old_size} to {requested} bytes",
                rid.0
            )));
        }
        let size = self.validate_new_allocation(size, old_size)?;
        let file = self.remote_files.get_mut(&rid).unwrap();
        file.data.resize(size, 0);
        self.allocated_file_size += size - old_size;
        Ok(size)
    }

    pub fn open_remote_pipe(&mut self, rid: Rid, kind: PipeOpenKind) -> Result<(), Error> {
        self.ensure_new_rid(rid)?;
        self.pipes.insert(
            rid,
            Pipe {
                access: kind.local_access(false),
                incoming: VecDeque::new(),
                read_eof: false,
                remote_read_closed: false,
                local_read_closed: false,
                local_write_closed: false,
                pending_open: None,
            },
        );
        self.fresh.insert(rid);
        Ok(())
    }

    pub fn create_local_pipe(&mut self, rid: Rid, kind: PipeOpenKind) -> Result<(), Error> {
        self.ensure_new_rid(rid)?;
        self.pipes.insert(
            rid,
            Pipe {
                access: kind.local_access(true),
                incoming: VecDeque::new(),
                read_eof: false,
                remote_read_closed: false,
                local_read_closed: false,
                local_write_closed: false,
                pending_open: Some(kind),
            },
        );
        self.fresh.insert(rid);
        Ok(())
    }

    pub fn inject_remote(&mut self, rid: Rid) -> Result<(), Error> {
        if !self.remote_files.contains_key(&rid) && !self.pipes.contains_key(&rid) {
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

    pub fn inject_local(&mut self, rid: Rid) -> Result<(), Error> {
        if !self.local_files.contains_key(&rid) && !self.pipes.contains_key(&rid) {
            return Err(Error::UnknownRid(rid));
        }
        if !self.fresh.remove(&rid) {
            return Err(Error::InvalidFrame(format!(
                "local RID {} is not ready for injection",
                rid.0
            )));
        }
        Ok(())
    }

    #[allow(clippy::single_range_in_vec_init)]
    pub fn apply_fill(
        &mut self,
        rid: Rid,
        payload: &[u8],
        compression: Compression,
    ) -> Result<Vec<Range<usize>>, Error> {
        if payload.len() < 8 {
            return Err(Error::InvalidFrame(
                "BufferFill payload is shorter than 8 bytes".to_string(),
            ));
        }
        let start = u32::from_le_bytes(payload[..4].try_into().unwrap()) as usize;
        let end = u32::from_le_bytes(payload[4..8].try_into().unwrap()) as usize;
        let expected = end
            .checked_sub(start)
            .ok_or_else(|| Error::InvalidFrame(format!("invalid fill range {start}..{end}")))?;
        if expected > self.max_transfer_size {
            return Err(Error::InvalidFrame(format!(
                "BufferFill size {expected} exceeds limit {}",
                self.max_transfer_size
            )));
        }
        let file = self.remote_files.get(&rid).ok_or(Error::UnknownRid(rid))?;
        if end > file.data.len() {
            return Err(Error::InvalidFrame(format!(
                "BufferFill range {start}..{end} exceeds file size {}",
                file.data.len()
            )));
        }
        let bytes = decompress(compression, &payload[8..], expected, self.max_transfer_size)?;
        let file = self.remote_files.get_mut(&rid).unwrap();
        file.data[start..end].copy_from_slice(&bytes);
        if start < end {
            file.record_damage(start..end);
            Ok(vec![start..end])
        } else {
            Ok(Vec::new())
        }
    }

    pub fn apply_diff(
        &mut self,
        rid: Rid,
        payload: &[u8],
        compression: Compression,
    ) -> Result<Vec<Range<usize>>, Error> {
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
            .ok_or_else(|| Error::InvalidFrame("diff size overflow".to_string()))?;
        let file = self.remote_files.get(&rid).ok_or(Error::UnknownRid(rid))?;
        if trailing_size > file.data.len() {
            return Err(Error::InvalidFrame(format!(
                "diff trailing size {trailing_size} exceeds file size {}",
                file.data.len()
            )));
        }
        let decoded = decompress(compression, &payload[8..], expected, self.max_transfer_size)?;
        let file = self.remote_files.get_mut(&rid).unwrap();
        let file_size = file.data.len();
        let (diff, trailing) = decoded.split_at(diff_size);
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
            let copy = &diff[data_start..data_start + copy_size];
            if file.data[byte_start..byte_end] != *copy {
                file.data[byte_start..byte_end].copy_from_slice(copy);
                file.record_damage(byte_start..byte_end);
                changed.push(byte_start..byte_end);
            }
            offset = data_start + copy_size;
        }

        if trailing_size > 0 {
            let start = file_size - trailing_size;
            if file.data[start..] != *trailing {
                file.data[start..].copy_from_slice(trailing);
                file.record_damage(start..file_size);
                changed.push(start..file_size);
            }
        }
        Ok(changed)
    }

    pub fn receive_pipe_data(&mut self, rid: Rid, data: &[u8]) -> Result<(), Error> {
        let Some(pipe) = self.pipes.get_mut(&rid) else {
            return Ok(());
        };
        if !pipe.access.can_read {
            return Err(Error::InvalidFrame(format!(
                "PipeTransfer sent to write-only local pipe {}",
                rid.0
            )));
        }
        if pipe.local_read_closed {
            return Ok(());
        }
        if pipe.read_eof {
            return Err(Error::InvalidFrame(format!(
                "PipeTransfer sent after shutdown on pipe {}",
                rid.0
            )));
        }
        let new_size = pipe
            .incoming
            .len()
            .checked_add(data.len())
            .ok_or_else(|| Error::InvalidFrame("pipe buffer size overflow".to_string()))?;
        if new_size > self.max_pipe_buffer_size {
            return Err(Error::InvalidFrame(format!(
                "pipe {} buffered {new_size} bytes, exceeding limit {}",
                rid.0, self.max_pipe_buffer_size
            )));
        }
        pipe.incoming.extend(data);
        Ok(())
    }

    pub fn receive_pipe_shutdown(&mut self, rid: Rid, kind: MessageType) -> Result<(), Error> {
        let Some(pipe) = self.pipes.get_mut(&rid) else {
            return Ok(());
        };
        match kind {
            MessageType::PipeShutdownW if pipe.access.can_read => pipe.read_eof = true,
            MessageType::PipeShutdownR if pipe.access.can_write => pipe.remote_read_closed = true,
            _ => {
                return Err(Error::InvalidFrame(format!(
                    "invalid {kind:?} for pipe {}",
                    rid.0
                )));
            }
        }
        Ok(())
    }

    pub fn read_pipe(&mut self, rid: Rid, max_bytes: usize) -> Result<PipeRead, Error> {
        let pipe = self.pipes.get_mut(&rid).ok_or(Error::UnknownRid(rid))?;
        let count = max_bytes.min(pipe.incoming.len());
        let data = pipe.incoming.drain(..count).collect();
        Ok(PipeRead {
            data,
            eof: pipe.read_eof && pipe.incoming.is_empty(),
        })
    }

    pub fn validate_pipe_write(&self, rid: Rid, len: usize) -> Result<(), Error> {
        let pipe = self.pipes.get(&rid).ok_or(Error::UnknownRid(rid))?;
        if !pipe.access.can_write || pipe.remote_read_closed || pipe.local_write_closed {
            return Err(Error::InvalidFrame(format!(
                "pipe {} is not open for local writes",
                rid.0
            )));
        }
        if len > self.max_transfer_size {
            return Err(Error::InvalidFrame(format!(
                "pipe write of {len} bytes exceeds transfer limit {}",
                self.max_transfer_size
            )));
        }
        Ok(())
    }

    pub fn close_local_pipe_read(&mut self, rid: Rid) -> Result<(), Error> {
        let pipe = self.pipes.get_mut(&rid).ok_or(Error::UnknownRid(rid))?;
        if !pipe.access.can_read || pipe.local_read_closed {
            return Err(Error::InvalidFrame(format!(
                "pipe {} is not open for local reads",
                rid.0
            )));
        }
        pipe.local_read_closed = true;
        pipe.incoming.clear();
        Ok(())
    }

    pub fn close_local_pipe_write(&mut self, rid: Rid) -> Result<(), Error> {
        self.validate_pipe_write(rid, 0)?;
        let pipe = self.pipes.get_mut(&rid).unwrap();
        pipe.local_write_closed = true;
        Ok(())
    }

    pub fn pipe_access(&self, rid: Rid) -> Option<PipeAccess> {
        self.pipes.get(&rid).map(|pipe| pipe.access)
    }

    pub fn local_access(&self, rid: Rid) -> Option<PipeAccess> {
        self.pipe_access(rid)
    }

    pub(crate) fn take_local_files(&mut self) -> Vec<(Rid, Arc<[u8]>)> {
        let mut files = Vec::new();
        for (rid, file) in &mut self.local_files {
            if !file.export_started {
                file.export_started = true;
                files.push((*rid, file.data.clone()));
            }
        }
        files
    }

    pub(crate) fn take_local_pipe_openings(&mut self) -> Vec<(Rid, PipeOpenKind)> {
        let mut openings = Vec::new();
        for (rid, pipe) in &mut self.pipes {
            if let Some(kind) = pipe.pending_open.take() {
                openings.push((*rid, kind));
            }
        }
        openings
    }

    pub fn close_resource(&mut self, rid: Rid) -> bool {
        self.fresh.remove(&rid);
        let file_size = self
            .remote_files
            .remove(&rid)
            .map(|file| file.data.len())
            .or_else(|| self.local_files.remove(&rid).map(|file| file.data.len()));
        if let Some(size) = file_size {
            self.allocated_file_size -= size;
        }
        let pipe_removed = self.pipes.remove(&rid).is_some();
        file_size.is_some() || pipe_removed
    }

    pub fn data(&self, rid: Rid) -> Option<&[u8]> {
        self.remote_files
            .get(&rid)
            .map(|file| file.data.as_slice())
            .or_else(|| self.local_files.get(&rid).map(|file| file.data.as_ref()))
    }

    pub fn damage(&self, rid: Rid) -> Option<&[Range<usize>]> {
        self.remote_files
            .get(&rid)
            .map(|file| file.damage.as_slice())
    }

    pub fn take_damage(&mut self, rid: Rid) -> Option<Vec<Range<usize>>> {
        self.remote_files
            .get_mut(&rid)
            .map(|file| std::mem::take(&mut file.damage))
    }

    pub fn clear_damage(&mut self) {
        for file in self.remote_files.values_mut() {
            file.damage.clear();
        }
    }

    pub fn contains(&self, rid: Rid) -> bool {
        self.remote_files.contains_key(&rid)
            || self.local_files.contains_key(&rid)
            || self.pipes.contains_key(&rid)
    }

    pub fn allocated_file_size(&self) -> usize {
        self.allocated_file_size
    }

    fn ensure_new_rid(&self, rid: Rid) -> Result<(), Error> {
        if self.contains(rid) || self.fresh.contains(&rid) {
            return Err(Error::DuplicateRid(rid));
        }
        Ok(())
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

    fn validate_new_allocation(&self, size: i32, old_size: usize) -> Result<usize, Error> {
        let size = self.validate_size(size)?;
        let total = self
            .allocated_file_size
            .checked_sub(old_size)
            .and_then(|remaining| remaining.checked_add(size))
            .ok_or_else(|| Error::InvalidFrame("shared-memory allocation overflow".to_string()))?;
        if total > self.max_total_file_size {
            return Err(Error::InvalidFrame(format!(
                "total shared-memory allocation {total} exceeds limit {}",
                self.max_total_file_size
            )));
        }
        Ok(size)
    }
}
