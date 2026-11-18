use std::io::Read;

use crate::error::Error;
use crate::header::Compression;

pub const DEFAULT_MAX_SHM_TRANSFER_SIZE: usize = 16 * 1024 * 1024;

pub fn decompress(
    compression: Compression,
    input: &[u8],
    expected_size: usize,
    max_size: usize,
) -> Result<Vec<u8>, Error> {
    if expected_size > max_size {
        return Err(Error::InvalidFrame(format!(
            "uncompressed transfer size {expected_size} exceeds limit {max_size}"
        )));
    }

    let output = match compression {
        Compression::None => {
            if input.len() != expected_size {
                return Err(Error::InvalidFrame(format!(
                    "uncompressed transfer has {} bytes, expected {expected_size}",
                    input.len()
                )));
            }
            input.to_vec()
        }
        Compression::Lz4 => lz4_flex::block::decompress(input, expected_size)
            .map_err(|error| Error::InvalidFrame(format!("invalid LZ4 transfer: {error}")))?,
        Compression::Zstd => {
            let decoder = zstd::stream::read::Decoder::new(input)
                .map_err(|error| Error::InvalidFrame(format!("invalid Zstd transfer: {error}")))?;
            let mut output = Vec::with_capacity(expected_size);
            let mut reader = decoder.take((expected_size as u64) + 1);
            reader
                .read_to_end(&mut output)
                .map_err(|error| Error::InvalidFrame(format!("invalid Zstd transfer: {error}")))?;
            output
        }
    };

    if output.len() != expected_size {
        return Err(Error::InvalidFrame(format!(
            "decompressed transfer has {} bytes, expected {expected_size}",
            output.len()
        )));
    }
    Ok(output)
}

pub fn compress(compression: Compression, input: &[u8]) -> Result<Vec<u8>, Error> {
    match compression {
        Compression::None => Ok(input.to_vec()),
        Compression::Lz4 => Ok(lz4_flex::block::compress(input)),
        Compression::Zstd => zstd::bulk::compress(input, 1)
            .map_err(|error| Error::InvalidFrame(format!("Zstd compression failed: {error}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_all_compression_modes() {
        let input = b"waypane-compression-waypane-compression".repeat(100);
        for compression in [Compression::None, Compression::Lz4, Compression::Zstd] {
            let encoded = compress(compression, &input).unwrap();
            let decoded = decompress(compression, &encoded, input.len(), input.len()).unwrap();
            assert_eq!(decoded, input);
        }
    }

    #[test]
    fn rejects_wrong_output_size_and_small_limits() {
        let encoded = compress(Compression::Zstd, b"hello").unwrap();
        assert!(decompress(Compression::Zstd, &encoded, 6, 16).is_err());
        assert!(matches!(
            decompress(Compression::None, b"hello", 5, 4),
            Err(Error::InvalidFrame(_))
        ));
        let encoded = lz4_flex::block::compress(&vec![0u8; 4096]);
        assert!(matches!(
            decompress(Compression::Lz4, &encoded, 4096, 4095),
            Err(Error::InvalidFrame(_))
        ));
    }
}
