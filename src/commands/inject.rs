use std::fs::File;
use std::io::{BufReader, BufWriter, Write, stdout};
use std::path::PathBuf;

use anyhow::{Result, bail};
use indicatif::ProgressBar;

use hdr10plus::av1::encode_av1_from_json;
use hdr10plus::metadata_json::{Hdr10PlusJsonMetadata, MetadataJsonRoot};

use crate::commands::{CliOptions, InjectArgs, input_from_either};
use crate::core::av1_parser::{
    Av1NaluParser, IvfFrameHeader, Obu, OBU_TEMPORAL_DELIMITER, is_hdr10plus_obu,
    read_ivf_frame_header, read_obus_from_ivf_frame, try_read_ivf_file_header,
    write_ivf_frame_header,
};
use crate::core::initialize_progress_bar;

pub struct Injector {
    input: PathBuf,
    json_in: PathBuf,
    options: CliOptions,
    output: PathBuf,
    progress_bar: ProgressBar,
}

impl Injector {
    pub fn inject_json(args: InjectArgs, cli_options: CliOptions) -> Result<()> {
        let InjectArgs {
            input,
            input_pos,
            json,
            output,
        } = args;

        let input = input_from_either("inject", input, input_pos)?;
        let output = output.unwrap_or_else(|| PathBuf::from("injected_output.av1"));

        let pb = initialize_progress_bar(&input)?;

        Injector {
            progress_bar: pb,
            json_in: json,
            options: cli_options,
            output,
            input,
        }
        .run()
    }

    fn run(&self) -> Result<()> {
        println!("Parsing JSON file...");
        stdout().flush().ok();

        let metadata_root = MetadataJsonRoot::from_file(&self.json_in)?;
        let metadata_list: Vec<Hdr10PlusJsonMetadata> = metadata_root.scene_info;

        if metadata_list.is_empty() {
            bail!("Empty HDR10+ SceneInfo array");
        }

        let file = File::open(&self.input)?;
        let mut reader = BufReader::with_capacity(100_000, file);

        let out_file = File::create(&self.output).expect("Can't create output file");
        let mut writer = BufWriter::with_capacity(100_000, out_file);

        // ── detect IVF vs raw ──────────────────────────────────────────────
        if let Some(ivf_header) = try_read_ivf_file_header(&mut reader)? {
            // Pass the file header straight through
            writer.write_all(&ivf_header)?;
            self.inject_ivf(&mut reader, &mut writer, &metadata_list)?;
        } else {
            self.inject_raw(&mut reader, &mut writer, &metadata_list)?;
        }

        self.progress_bar.finish_and_clear();
        println!("Rewriting with interleaved HDR10+ metadata OBUs: Done.");
        writer.flush()?;
        Ok(())
    }

    // ── IVF injection ──────────────────────────────────────────────────────
    //
    // Each IVF frame == one temporal unit.
    // Read frame_size bytes, parse OBUs, inject HDR10+, write back.

    fn inject_ivf<R, W>(
        &self,
        reader: &mut R,
        writer: &mut W,
        metadata_list: &[Hdr10PlusJsonMetadata],
    ) -> Result<()>
    where
        R: std::io::Read,
        W: Write,
    {
        let total_meta = metadata_list.len();
        let mut tu_index = 0usize;
        let mut last_encoded: Option<Vec<u8>> = None;
        let mut warned_existing = false;
        let mut warned_mismatch = false;

        loop {
            let fh: IvfFrameHeader = match read_ivf_frame_header(reader)? {
                Some(h) => h,
                None => break,
            };

            let mut frame_data = vec![0u8; fh.frame_size as usize];
            reader.read_exact(&mut frame_data)?;

            let obus = read_obus_from_ivf_frame(frame_data)?;

            // Warn about existing HDR10+ on first occurrence
            if !warned_existing && obus.iter().any(|o| is_hdr10plus_obu(o, self.options.validate)) {
                warned_existing = true;
                println!(
                    "\nWarning: Input file already has HDR10+ metadata OBUs; \
                     they will be replaced."
                );
            }

            // Encode the right metadata for this TU
            let encoded = if tu_index < total_meta {
                let enc = encode_av1_from_json(&metadata_list[tu_index], self.options.validate)?;
                last_encoded = Some(enc.clone());
                enc
            } else {
                if !warned_mismatch {
                    warned_mismatch = true;
                    println!(
                        "\nWarning: mismatched lengths. \
                         Metadata has {total_meta} entries but video has more frames. \
                         Last metadata will be duplicated."
                    );
                }
                match &last_encoded {
                    Some(enc) => enc.clone(),
                    None => bail!("No HDR10+ metadata available for TU {tu_index}"),
                }
            };

            // Build output frame OBUs
            let output_frame = Self::build_output_frame(&obus, &encoded, self.options.validate);

            // Write IVF frame header + data
            write_ivf_frame_header(writer, output_frame.len() as u32, fh.timestamp)?;
            writer.write_all(&output_frame)?;

            tu_index += 1;
        }

        if tu_index < total_meta {
            println!(
                "\nWarning: mismatched lengths. Metadata has {total_meta} entries \
                 but video has {tu_index} frames. Excess metadata was ignored."
            );
        }

        Ok(())
    }

    // ── Raw OBU stream injection ───────────────────────────────────────────
    //
    // Temporal units are delimited by OBU_TEMPORAL_DELIMITER.
    // We buffer one TU at a time, flush with injected HDR10+ on the next TD.

    fn inject_raw<R, W>(
        &self,
        reader: &mut R,
        writer: &mut W,
        metadata_list: &[Hdr10PlusJsonMetadata],
    ) -> Result<()>
    where
        R: std::io::Read,
        W: Write,
    {
        let mut av1_parser = Av1NaluParser::new();
        let total_meta = metadata_list.len();
        let mut tu_index = 0usize;
        let mut last_encoded: Option<Vec<u8>> = None;
        let mut warned_existing = false;
        let mut warned_mismatch = false;

        let mut current_td: Option<Obu> = None;
        let mut pending: Vec<Obu> = Vec::new();

        loop {
            let obu_opt = Obu::read_from(reader)?;
            let is_eof = obu_opt.is_none();
            let is_td = obu_opt
                .as_ref()
                .map(|o| o.obu_type == OBU_TEMPORAL_DELIMITER)
                .unwrap_or(false);

            if (is_eof || is_td) && current_td.is_some() {
                // Flush current TU
                if !warned_existing
                    && pending
                        .iter()
                        .any(|o| is_hdr10plus_obu(o, self.options.validate))
                {
                    warned_existing = true;
                    println!(
                        "\nWarning: Input file already has HDR10+ metadata OBUs; \
                         they will be replaced."
                    );
                }

                let encoded = if tu_index < total_meta {
                    let enc =
                        encode_av1_from_json(&metadata_list[tu_index], self.options.validate)?;
                    last_encoded = Some(enc.clone());
                    enc
                } else {
                    if !warned_mismatch {
                        warned_mismatch = true;
                        println!(
                            "\nWarning: mismatched lengths. \
                             Metadata has {total_meta} entries but video has more frames. \
                             Last metadata will be duplicated."
                        );
                    }
                    match &last_encoded {
                        Some(enc) => enc.clone(),
                        None => bail!("No HDR10+ metadata available for TU {tu_index}"),
                    }
                };

                // Write: TD + HDR10+ metadata + remaining OBUs (skip existing HDR10+)
                let td = current_td.take().unwrap();
                writer.write_all(&td.raw_bytes)?;
                writer.write_all(&encoded)?;
                for obu in pending.drain(..) {
                    if !is_hdr10plus_obu(&obu, self.options.validate) {
                        writer.write_all(&obu.raw_bytes)?;
                    }
                }

                tu_index += 1;
            }

            match obu_opt {
                None => break,
                Some(obu) => {
                    av1_parser.process_obu(&obu)?;
                    if obu.obu_type == OBU_TEMPORAL_DELIMITER {
                        current_td = Some(obu);
                        pending.clear();
                    } else if current_td.is_some() {
                        pending.push(obu);
                    } else {
                        // OBUs before the first TD — pass through unchanged
                        writer.write_all(&obu.raw_bytes)?;
                    }
                }
            }
        }

        if tu_index < total_meta {
            println!(
                "\nWarning: mismatched lengths. Metadata has {total_meta} entries \
                 but video has {tu_index} frames. Excess metadata was ignored."
            );
        }

        Ok(())
    }

    // ── Shared helper ──────────────────────────────────────────────────────

    /// Build the output byte buffer for one temporal unit's OBUs:
    /// inject `encoded` right after the OBU_TEMPORAL_DELIMITER (if present)
    /// and strip any existing HDR10+ OBUs.
    fn build_output_frame(obus: &[Obu], encoded: &[u8], validate: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let mut injected = false;

        // Insertion point: right after OBU_TEMPORAL_DELIMITER, or at position 0
        let insert_after_td = obus
            .iter()
            .position(|o| o.obu_type == OBU_TEMPORAL_DELIMITER)
            .map(|i| i + 1)
            .unwrap_or(0);

        for (i, obu) in obus.iter().enumerate() {
            if !injected && i == insert_after_td {
                out.extend_from_slice(encoded);
                injected = true;
            }
            if is_hdr10plus_obu(obu, validate) {
                continue; // drop existing HDR10+
            }
            out.extend_from_slice(&obu.raw_bytes);
        }

        if !injected {
            out.extend_from_slice(encoded);
        }

        out
    }
}
