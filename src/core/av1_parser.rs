#![allow(dead_code)]

use std::io::{BufRead, ErrorKind, Read, Write};

use anyhow::{Result, bail};
use bitvec_helpers::bitstream_io_reader::BsIoSliceReader;

use hdr10plus::av1::{decode_leb128, encode_leb128};

// ---------------------------------------------------------------------------
// OBU type constants (AV1 spec Table 5)
// ---------------------------------------------------------------------------
pub const OBU_SEQUENCE_HEADER: u8 = 1;
pub const OBU_TEMPORAL_DELIMITER: u8 = 2;
pub const OBU_FRAME_HEADER: u8 = 3;
pub const OBU_METADATA: u8 = 5;
pub const OBU_FRAME: u8 = 6;
pub const OBU_REDUNDANT_FRAME_HEADER: u8 = 7;

// Metadata type for ITU-T T.35 (HDR10+)
pub const METADATA_TYPE_ITUT_T35: u64 = 4;

// HDR10+ T.35 header identifiers
pub const HDR10PLUS_COUNTRY_CODE: u8 = 0xB5;
pub const HDR10PLUS_PROVIDER_CODE: u16 = 0x003C;
pub const HDR10PLUS_ORIENTED_CODE: u16 = 0x0001;
pub const HDR10PLUS_APP_ID: u8 = 4;

// ---------------------------------------------------------------------------
// Obu — a single parsed OBU with its complete raw bytes
// ---------------------------------------------------------------------------

/// A single parsed AV1 Open Bitstream Unit.
pub struct Obu {
    pub obu_type: u8,
    pub temporal_id: u8,
    pub spatial_id: u8,
    /// Decoded payload bytes (after header + LEB128 size).
    pub payload: Vec<u8>,
    /// Complete raw bytes of this OBU as it appeared on disk
    /// (header byte(s) + LEB128 size + payload).
    /// Used for pass-through writing.
    pub raw_bytes: Vec<u8>,
}

impl Obu {
    /// Read one OBU from `reader`.  Returns `None` on clean EOF.
    ///
    /// Only supports the *Low Overhead Bitstream Format* where every OBU
    /// carries a size field (`obu_has_size_field == 1`).
    pub fn read_from<R: Read>(reader: &mut R) -> Result<Option<Self>> {
        // ---- header byte ----
        let mut header_byte = [0u8; 1];
        match reader.read_exact(&mut header_byte) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }

        let byte = header_byte[0];
        if byte >> 7 != 0 {
            bail!("AV1 OBU forbidden bit is set (byte = 0x{byte:02X})");
        }

        let obu_type = (byte >> 3) & 0x0F;
        let has_extension = (byte >> 2) & 1 != 0;
        let has_size_field = (byte >> 1) & 1 != 0;

        let mut raw = vec![byte];
        let mut temporal_id = 0u8;
        let mut spatial_id = 0u8;

        // ---- optional extension header ----
        if has_extension {
            let mut ext = [0u8; 1];
            reader.read_exact(&mut ext)?;
            temporal_id = (ext[0] >> 5) & 0x07;
            spatial_id = (ext[0] >> 3) & 0x03;
            raw.push(ext[0]);
        }

        if !has_size_field {
            bail!(
                "OBU (type {obu_type}) has no size field; \
                 only Low Overhead Bitstream Format is supported"
            );
        }

        // ---- LEB128 payload size ----
        let payload_size = {
            let mut size: u64 = 0;
            let mut shift = 0u32;
            loop {
                let mut b = [0u8; 1];
                reader.read_exact(&mut b)?;
                raw.push(b[0]);
                size |= ((b[0] & 0x7F) as u64) << shift;
                shift += 7;
                if b[0] & 0x80 == 0 {
                    break;
                }
                if shift >= 56 {
                    bail!("LEB128 overflow while reading OBU size");
                }
            }
            size as usize
        };

        // ---- payload ----
        let payload_start = raw.len();
        raw.resize(payload_start + payload_size, 0);
        reader.read_exact(&mut raw[payload_start..])?;
        let payload = raw[payload_start..].to_vec();

        Ok(Some(Obu {
            obu_type,
            temporal_id,
            spatial_id,
            payload,
            raw_bytes: raw,
        }))
    }

    /// Rebuild `raw_bytes` from a (possibly modified) payload.
    /// Used when we need to re-encode an OBU after touching its payload.
    pub fn rebuild_raw(&self) -> Vec<u8> {
        // header byte: re-use original (type, flags already set; has_size_field=1)
        let header_byte = self.raw_bytes[0];
        let has_extension = (header_byte >> 2) & 1 != 0;

        let size_bytes = encode_leb128(self.payload.len() as u64);

        let header_len = if has_extension { 2 } else { 1 };
        let mut out = Vec::with_capacity(header_len + size_bytes.len() + self.payload.len());
        out.extend_from_slice(&self.raw_bytes[..header_len]);
        out.extend_from_slice(&size_bytes);
        out.extend_from_slice(&self.payload);
        out
    }
}

// ---------------------------------------------------------------------------
// HDR10+ detection helper
// ---------------------------------------------------------------------------

/// Returns the T.35 payload bytes (starting at country_code = 0xB5) if this
/// OBU_METADATA payload contains HDR10+ data.  The returned slice is the
/// portion that `Hdr10PlusMetadata::parse()` expects.
///
/// Layout of an OBU_METADATA payload for HDR10+:
/// ```text
/// metadata_type  (LEB128)  = 4
/// country_code   (u8)      = 0xB5
/// provider_code  (u16 BE)  = 0x003C
/// oriented_code  (u16 BE)  = 0x0001
/// app_id         (u8)      = 4
/// app_version    (u8)      = 1
/// <HDR10+ payload bits>
/// ```
pub fn extract_hdr10plus_t35_bytes(
    obu_payload: &[u8],
    validate: bool,
) -> Option<Vec<u8>> {
    if obu_payload.is_empty() {
        return None;
    }

    // metadata_type
    let (metadata_type, mt_len) = decode_leb128(obu_payload);
    if metadata_type != METADATA_TYPE_ITUT_T35 {
        return None;
    }

    let t35 = &obu_payload[mt_len..];
    if t35.len() < 7 {
        return None;
    }

    let country_code = t35[0];
    if country_code != HDR10PLUS_COUNTRY_CODE {
        return None;
    }

    let provider_code = u16::from_be_bytes([t35[1], t35[2]]);
    let oriented_code = u16::from_be_bytes([t35[3], t35[4]]);
    let app_id = t35[5];
    let app_version = t35[6];

    if provider_code != HDR10PLUS_PROVIDER_CODE
        || oriented_code != HDR10PLUS_ORIENTED_CODE
        || app_id != HDR10PLUS_APP_ID
    {
        return None;
    }

    let valid_version = if validate {
        app_version == 1
    } else {
        app_version <= 1
    };

    if !valid_version {
        return None;
    }

    // Return T.35 bytes starting at country_code (what parse() expects)
    Some(t35.to_vec())
}

/// Returns `true` if this OBU is an OBU_METADATA carrying HDR10+ T.35 data.
pub fn is_hdr10plus_obu(obu: &Obu, validate: bool) -> bool {
    obu.obu_type == OBU_METADATA
        && extract_hdr10plus_t35_bytes(&obu.payload, validate).is_some()
}

// ---------------------------------------------------------------------------
// Stateful AV1 parser
// ---------------------------------------------------------------------------

/// Information about a single temporal unit derived from stream parsing.
#[derive(Debug, Clone)]
pub struct TemporalUnitInfo {
    /// Zero-based index of this temporal unit in the stream.
    pub index: usize,
    /// `true` when the temporal unit produces a visible output frame.
    /// This is `false` only for frames with `show_frame = 0` and
    /// `showable_frame = 0`.
    pub is_displayed: bool,
    /// `true` when the temporal unit reuses a previously decoded frame
    /// via `show_existing_frame`.
    pub is_show_existing: bool,
}

/// Stateful AV1 bitstream parser.
///
/// Feed it each `Obu` in stream order via [`process_obu`].  After parsing the
/// entire stream, [`temporal_units`] provides per-TU display metadata needed
/// for frame-accurate HDR10+ injection.
pub struct Av1NaluParser {
    /// Derived from the sequence header; affects frame header interpretation.
    pub reduced_still_picture_header: bool,

    /// Running count of temporal units seen so far (incremented on each TD).
    pub temporal_unit_count: usize,

    /// Per-temporal-unit display info (populated as frame headers are parsed).
    pub temporal_units: Vec<TemporalUnitInfo>,

    /// A frame header was already parsed in the current TU (to skip redundant ones).
    frame_header_parsed_in_tu: bool,
}

impl Av1NaluParser {
    pub fn new() -> Self {
        Self {
            reduced_still_picture_header: false,
            temporal_unit_count: 0,
            temporal_units: Vec::new(),
            frame_header_parsed_in_tu: false,
        }
    }

    /// Process one OBU and update parser state.
    pub fn process_obu(&mut self, obu: &Obu) -> Result<()> {
        match obu.obu_type {
            OBU_TEMPORAL_DELIMITER => {
                // Marks the beginning of a new temporal unit.
                self.temporal_unit_count += 1;
                self.frame_header_parsed_in_tu = false;
            }
            OBU_SEQUENCE_HEADER => {
                self.parse_sequence_header(&obu.payload)?;
            }
            OBU_FRAME_HEADER | OBU_FRAME => {
                if !self.frame_header_parsed_in_tu {
                    let info = self.parse_frame_display_info(&obu.payload)?;
                    let tu_idx = self.temporal_unit_count.saturating_sub(1);
                    self.temporal_units.push(TemporalUnitInfo {
                        index: tu_idx,
                        is_displayed: info.0,
                        is_show_existing: info.1,
                    });
                    self.frame_header_parsed_in_tu = true;
                }
            }
            OBU_REDUNDANT_FRAME_HEADER => {
                // Redundant copies carry the same info — skip to avoid duplicates.
            }
            _ => {}
        }
        Ok(())
    }

    /// Return all collected temporal unit infos.
    pub fn temporal_units(&self) -> &[TemporalUnitInfo] {
        &self.temporal_units
    }

    /// Returns the number of temporal units that produce a displayed frame.
    pub fn display_frame_count(&self) -> usize {
        self.temporal_units.iter().filter(|t| t.is_displayed).count()
    }

    // -----------------------------------------------------------------------
    // Sequence header parsing
    // -----------------------------------------------------------------------

    fn parse_sequence_header(&mut self, payload: &[u8]) -> Result<()> {
        if payload.len() < 1 {
            return Ok(());
        }
        let mut r = BsIoSliceReader::from_slice(payload);

        // seq_profile (3 bits)
        let _seq_profile = r.read::<3, u8>()?;
        // still_picture (1 bit)
        let _still_picture = r.read::<1, u8>()?;
        // reduced_still_picture_header (1 bit)
        self.reduced_still_picture_header = r.read::<1, u8>()? != 0;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Frame header parsing (minimal — only what is needed for display flags)
    // -----------------------------------------------------------------------

    /// Returns `(is_displayed, is_show_existing)`.
    fn parse_frame_display_info(&self, payload: &[u8]) -> Result<(bool, bool)> {
        if self.reduced_still_picture_header {
            // Section 5.9.2: reduced_still_picture_header implies
            // show_existing_frame = 0, show_frame = 1.
            return Ok((true, false));
        }

        if payload.is_empty() {
            return Ok((true, false));
        }

        let mut r = BsIoSliceReader::from_slice(payload);

        // show_existing_frame (f(1))
        let show_existing_frame = r.read::<1, u8>()? != 0;
        if show_existing_frame {
            return Ok((true, true));
        }

        // frame_type (f(2))
        let _frame_type = r.read::<2, u8>()?;

        // show_frame (f(1))
        let show_frame = r.read::<1, u8>()? != 0;

        if show_frame {
            Ok((true, false))
        } else {
            // showable_frame (f(1))
            let showable_frame = r.read::<1, u8>()? != 0;
            Ok((showable_frame, false))
        }
    }
}

// ---------------------------------------------------------------------------
// IVF container support
// ---------------------------------------------------------------------------

/// IVF file signature ("DKIF").
pub const IVF_SIGNATURE: [u8; 4] = *b"DKIF";

/// Size of the IVF file header in bytes.
pub const IVF_FILE_HEADER_LEN: usize = 32;

/// Size of an IVF frame header in bytes.
pub const IVF_FRAME_HEADER_LEN: usize = 12;

/// Header of a single IVF frame.
pub struct IvfFrameHeader {
    /// Number of bytes in the frame data that follows.
    pub frame_size: u32,
    /// Presentation timestamp (in stream timebase).
    pub timestamp: u64,
}

/// Probe the first bytes of `reader` to decide whether the stream is an IVF
/// container.  If the IVF signature is detected the 32-byte file header is
/// consumed from `reader` and returned; otherwise `None` is returned and
/// **no bytes are consumed**.
pub fn try_read_ivf_file_header<R: BufRead>(reader: &mut R) -> Result<Option<[u8; IVF_FILE_HEADER_LEN]>> {
    {
        let buf = reader.fill_buf()?;
        if buf.len() < 4 || buf[..4] != IVF_SIGNATURE {
            return Ok(None);
        }
    }
    let mut header = [0u8; IVF_FILE_HEADER_LEN];
    reader.read_exact(&mut header)?;
    Ok(Some(header))
}

/// Read one IVF frame header from `reader`.  Returns `None` on clean EOF.
pub fn read_ivf_frame_header<R: Read>(reader: &mut R) -> Result<Option<IvfFrameHeader>> {
    let mut buf = [0u8; IVF_FRAME_HEADER_LEN];
    match reader.read_exact(&mut buf) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    Ok(Some(IvfFrameHeader {
        frame_size: u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
        timestamp: u64::from_le_bytes(buf[4..12].try_into().unwrap()),
    }))
}

/// Write an IVF frame header (frame_size + timestamp) to `writer`.
pub fn write_ivf_frame_header<W: Write>(
    writer: &mut W,
    frame_size: u32,
    timestamp: u64,
) -> Result<()> {
    writer.write_all(&frame_size.to_le_bytes())?;
    writer.write_all(&timestamp.to_le_bytes())?;
    Ok(())
}

/// Read all OBUs from a single IVF frame's data bytes.
///
/// `frame_data` must contain exactly the bytes specified by the IVF frame
/// header's `frame_size` field.  OBUs inside IVF frames use the Low Overhead
/// Bitstream Format (each OBU carries `obu_has_size_field = 1`).
pub fn read_obus_from_ivf_frame(frame_data: Vec<u8>) -> Result<Vec<Obu>> {
    let mut cursor = std::io::Cursor::new(frame_data);
    let mut obus = Vec::new();
    while let Some(obu) = Obu::read_from(&mut cursor)? {
        obus.push(obu);
    }
    Ok(obus)
}
