use anyhow::{Result, ensure};
use bitvec_helpers::bitstream_io_writer::BitstreamIoWriter;

use crate::metadata::{Hdr10PlusMetadata, Hdr10PlusMetadataEncOpts};
use crate::metadata_json::Hdr10PlusJsonMetadata;

const OBU_METADATA_TYPE_HDR10_PLUS: u8 = 4;
const OBU_METADATA: u8 = 5;

fn write_uleb128(value: usize) -> Vec<u8> {
    let mut remaining = value;
    let mut out = Vec::new();

    loop {
        let mut byte = (remaining & 0x7F) as u8;
        remaining >>= 7;

        if remaining != 0 {
            byte |= 0x80;
        }

        out.push(byte);

        if remaining == 0 {
            break;
        }
    }

    out
}

fn encode_av1_metadata_payload(metadata: &Hdr10PlusMetadata, validate: bool) -> Result<Vec<u8>> {
    let opts = Hdr10PlusMetadataEncOpts {
        validate,
        with_country_code: true,
    };

    let mut writer = BitstreamIoWriter::with_capacity(64);

    // metadata_type = ITU-T T.35
    writer.write::<8, u8>(OBU_METADATA_TYPE_HDR10_PLUS)?;

    for byte in metadata.encode_with_opts(&opts)? {
        writer.write::<8, u8>(byte)?;
    }

    Ok(writer.into_inner())
}

/// Returns the raw OBU bitstream for a HDR10+ metadata message.
pub fn encode_av1_metadata_obu(metadata: &Hdr10PlusMetadata, validate: bool) -> Result<Vec<u8>> {
    let payload = encode_av1_metadata_payload(metadata, validate)?;

    // obu_forbidden_bit(1) obu_type(4) obu_extension_flag(1) obu_has_size_field(1) obu_reserved_1bit(1)
    let obu_header = (OBU_METADATA << 3) | 0x02; // has_size_field set

    let mut data = vec![obu_header];
    data.extend(write_uleb128(payload.len()));
    data.extend(payload);

    ensure!(
        data.len() <= u32::MAX as usize,
        "OBU too large: {} bytes",
        data.len()
    );

    Ok(data)
}

pub fn encode_av1_from_json(metadata: &Hdr10PlusJsonMetadata, validate: bool) -> Result<Vec<u8>> {
    let meta = Hdr10PlusMetadata::try_from(metadata)?;
    encode_av1_metadata_obu(&meta, validate)
}
