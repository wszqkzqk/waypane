use crate::error::Error;

pub const MIN_PROTOCOL_VERSION: u32 = 0x10;
pub const WAYPIPE_PROTOCOL_VERSION: u32 = 0x11;
pub const HEADER_SIZE: usize = 16;

const CONN_FIXED_BIT: u32 = 1 << 7;
const CONN_UNSET_BIT: u32 = 1 << 31;
const CONN_NO_DMABUF_SUPPORT: u32 = 1 << 2;
const CONN_COMPRESSION_MASK: u32 = 0x7 << 8;
const CONN_NO_COMPRESSION: u32 = 1 << 8;
const CONN_LZ4_COMPRESSION: u32 = 2 << 8;
const CONN_ZSTD_COMPRESSION: u32 = 3 << 8;
const CONN_VIDEO_MASK: u32 = 0x7 << 11;
const CONN_NO_VIDEO: u32 = 1 << 11;
const CONN_VP9_VIDEO: u32 = 2 << 11;
const CONN_H264_VIDEO: u32 = 3 << 11;
const CONN_AV1_VIDEO: u32 = 4 << 11;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Lz4,
    Zstd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    Vp9,
    H264,
    Av1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionHeader {
    pub remote_version: u32,
    pub version: u32,
    pub compression: Compression,
    pub has_dmabuf: bool,
    pub video: Option<VideoCodec>,
}

impl ConnectionHeader {
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < HEADER_SIZE {
            return Err(Error::InvalidHeader("truncated handshake"));
        }
        let lead = u32::from_le_bytes(bytes[..4].try_into().unwrap());
        if lead & CONN_FIXED_BIT == 0 {
            return Err(Error::InvalidHeader("fixed bit is missing"));
        }
        if lead & CONN_UNSET_BIT != 0 {
            return Err(Error::InvalidHeader("unset bit is present"));
        }

        let remote_version = (((lead >> 16) & 0xff) << 4) | ((lead >> 3) & 0xf);
        if !(MIN_PROTOCOL_VERSION..=WAYPIPE_PROTOCOL_VERSION).contains(&remote_version) {
            return Err(Error::ProtocolVersion(remote_version));
        }

        let compression = match lead & CONN_COMPRESSION_MASK {
            CONN_NO_COMPRESSION => Compression::None,
            CONN_LZ4_COMPRESSION => Compression::Lz4,
            CONN_ZSTD_COMPRESSION => Compression::Zstd,
            _ => return Err(Error::InvalidHeader("invalid compression field")),
        };
        let video = match lead & CONN_VIDEO_MASK {
            CONN_NO_VIDEO => None,
            CONN_VP9_VIDEO => Some(VideoCodec::Vp9),
            CONN_H264_VIDEO => Some(VideoCodec::H264),
            CONN_AV1_VIDEO => Some(VideoCodec::Av1),
            _ => return Err(Error::InvalidHeader("invalid video field")),
        };

        Ok(Self {
            remote_version,
            version: remote_version.min(WAYPIPE_PROTOCOL_VERSION),
            compression,
            has_dmabuf: lead & CONN_NO_DMABUF_SUPPORT == 0,
            video,
        })
    }
}

#[cfg(test)]
pub(crate) fn server_header(compression: Compression, no_dmabuf: bool) -> [u8; HEADER_SIZE] {
    let version = WAYPIPE_PROTOCOL_VERSION;
    let mut lead = ((version >> 4) << 16) | ((version & 0xf) << 3) | CONN_FIXED_BIT | CONN_NO_VIDEO;
    lead |= match compression {
        Compression::None => CONN_NO_COMPRESSION,
        Compression::Lz4 => CONN_LZ4_COMPRESSION,
        Compression::Zstd => CONN_ZSTD_COMPRESSION,
    };
    if no_dmabuf {
        lead |= CONN_NO_DMABUF_SUPPORT;
    }

    let mut bytes = [0u8; HEADER_SIZE];
    bytes[..4].copy_from_slice(&lead.to_le_bytes());
    bytes
}
