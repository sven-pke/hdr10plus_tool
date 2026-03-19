use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;

use anyhow::Result;
use indicatif::ProgressBar;

use crate::commands::{CliOptions, RemoveArgs, input_from_either};
use crate::core::av1_parser::{
    Obu, is_hdr10plus_obu, read_ivf_frame_header, read_obus_from_ivf_frame,
    try_read_ivf_file_header, write_ivf_frame_header,
};
use crate::core::initialize_progress_bar;

pub struct Remover {
    input: PathBuf,
    output: PathBuf,
    progress_bar: ProgressBar,
    validate: bool,
}

impl Remover {
    pub fn remove_sei(args: RemoveArgs, options: CliOptions) -> Result<()> {
        let RemoveArgs {
            input,
            input_pos,
            output,
        } = args;
        let input = input_from_either("remove", input, input_pos)?;
        let output = output.unwrap_or_else(|| PathBuf::from("hdr10plus_removed_output.av1"));

        let pb = initialize_progress_bar(&input)?;

        Remover {
            progress_bar: pb,
            output,
            validate: options.validate,
            input,
        }
        .run()
    }

    fn run(&self) -> Result<()> {
        let file = File::open(&self.input)?;
        let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let mut reader = BufReader::with_capacity(100_000, file);

        let out_file = File::create(&self.output).expect("Can't create output file");
        let mut writer = BufWriter::with_capacity(100_000, out_file);

        let mut bytes_read = 0u64;

        if let Some(ivf_header) = try_read_ivf_file_header(&mut reader)? {
            // IVF: pass file header through, then remove HDR10+ OBUs per frame
            writer.write_all(&ivf_header)?;
            bytes_read += ivf_header.len() as u64;

            loop {
                let fh = match read_ivf_frame_header(&mut reader)? {
                    Some(h) => h,
                    None => break,
                };
                bytes_read += 12;
                self.progress_bar
                    .set_position(bytes_read * 100 / file_len.max(1));

                let mut frame_data = vec![0u8; fh.frame_size as usize];
                reader.read_exact(&mut frame_data)?;
                bytes_read += fh.frame_size as u64;

                let obus = read_obus_from_ivf_frame(frame_data)?;

                // Collect output OBUs (skip HDR10+)
                let output_frame: Vec<u8> = obus
                    .iter()
                    .filter(|o| !is_hdr10plus_obu(o, self.validate))
                    .flat_map(|o| o.raw_bytes.iter().copied())
                    .collect();

                write_ivf_frame_header(&mut writer, output_frame.len() as u32, fh.timestamp)?;
                writer.write_all(&output_frame)?;
            }
        } else {
            // Raw OBU stream: skip HDR10+ OBUs, copy everything else
            loop {
                match Obu::read_from(&mut reader) {
                    Ok(Some(obu)) => {
                        bytes_read += obu.raw_bytes.len() as u64;
                        self.progress_bar
                            .set_position(bytes_read * 100 / file_len.max(1));

                        if !is_hdr10plus_obu(&obu, self.validate) {
                            writer.write_all(&obu.raw_bytes)?;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => return Err(e),
                }
            }
        }

        self.progress_bar.finish_and_clear();
        writer.flush()?;
        Ok(())
    }
}
