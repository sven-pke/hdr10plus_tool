use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write, stdout};
use std::path::PathBuf;

use anyhow::{Result, bail, ensure};
use hevc_parser::utils::{
    add_start_code_emulation_prevention_3_byte, clear_start_code_emulation_prevention_3_byte,
};
use indicatif::ProgressBar;

use hevc_parser::io::{FrameBuffer, IoFormat, IoProcessor, NalBuffer, processor};
use hevc_parser::{HevcParser, NALUStartCode, hevc::*};
use processor::{HevcProcessor, HevcProcessorOpts};

use hdr10plus::av1::encode_av1_from_json;
use hdr10plus::metadata_json::{Hdr10PlusJsonMetadata, MetadataJsonRoot};

use crate::commands::InjectArgs;
use crate::core::{initialize_progress_bar, st2094_40_sei_msg};

use super::{CliOptions, input_from_either};

pub struct Injector {
    input: PathBuf,
    json_in: PathBuf,
    options: CliOptions,

    metadata_list: Vec<Hdr10PlusJsonMetadata>,

    writer: BufWriter<File>,
    progress_bar: ProgressBar,
    already_checked_for_hdr10plus: bool,

    frames: Vec<Frame>,
    nals: Vec<NALUnit>,
    mismatched_length: bool,

    frame_buffer: FrameBuffer,
    last_metadata_written: Option<NalBuffer>,
}

struct IvfHeader {
    raw: [u8; 32],
    frame_count: u32,
    timebase_den: u32,
    timebase_num: u32,
}

struct Av1Injector {
    input: PathBuf,
    options: CliOptions,
    metadata_list: Vec<Hdr10PlusJsonMetadata>,
    progress_bar: ProgressBar,
    writer: BufWriter<File>,
    mismatched_length: bool,
}

impl Injector {
    pub fn from_args(args: InjectArgs, cli_options: CliOptions) -> Result<Self> {
        let InjectArgs {
            input,
            input_pos,
            json,
            output,
        } = args;

        let input = input_from_either("inject", input, input_pos)?;

        let output = match output {
            Some(path) => path,
            None => PathBuf::from("injected_output.hevc"),
        };

        let chunk_size = 100_000;
        let progress_bar = initialize_progress_bar(&IoFormat::Raw, &input)?;

        let writer =
            BufWriter::with_capacity(chunk_size, File::create(output).expect("Can't create file"));

        let mut injector = Injector {
            input,
            json_in: json,
            options: cli_options,
            metadata_list: Vec::new(),

            writer,
            progress_bar,
            already_checked_for_hdr10plus: false,

            frames: Vec::new(),
            nals: Vec::new(),
            mismatched_length: false,

            frame_buffer: FrameBuffer {
                frame_number: 0,
                nals: Vec::with_capacity(16),
            },
            last_metadata_written: None,
        };

        println!("Parsing JSON file...");
        stdout().flush().ok();

        let metadata_root = MetadataJsonRoot::from_file(&injector.json_in)?;
        injector.metadata_list = metadata_root.scene_info;

        if injector.metadata_list.is_empty() {
            bail!("Empty HDR10+ SceneInfo array");
        }

        Ok(injector)
    }

    pub fn inject_json(args: InjectArgs, cli_options: CliOptions) -> Result<()> {
        let input = input_from_either("inject", args.input.clone(), args.input_pos.clone())?;

        if let Ok(format) = hevc_parser::io::format_from_path(&input) {
            if let IoFormat::Raw = format {
                let mut injector = Injector::from_args(args, cli_options)?;

                injector.process_input()?;
                return injector.interleave_hdr10plus_nals();
            }
        }

        if input.extension().map(|ext| ext.eq_ignore_ascii_case("ivf")) == Some(true) {
            let mut injector = Av1Injector::from_args(args, cli_options)?;
            injector.inject_ivf()
        } else {
            bail!("Injector: Input must be a raw HEVC bitstream or AV1 IVF file")
        }
    }

    fn process_input(&mut self) -> Result<()> {
        println!("Processing input video for frame order info...");
        stdout().flush().ok();

        let chunk_size = 100_000;

        let mut processor =
            HevcProcessor::new(IoFormat::Raw, HevcProcessorOpts::default(), chunk_size);

        let file = File::open(&self.input)?;
        let mut reader = Box::new(BufReader::with_capacity(100_000, file));

        processor.process_io(&mut reader, self)
    }

    fn interleave_hdr10plus_nals(&mut self) -> Result<()> {
        let metadata_list = &self.metadata_list;
        self.mismatched_length = if self.frames.len() != metadata_list.len() {
            println!(
                "\nWarning: mismatched lengths. video {}, HDR10+ JSON {}",
                self.frames.len(),
                metadata_list.len()
            );

            if metadata_list.len() < self.frames.len() {
                println!("Metadata will be duplicated at the end to match video length\n");
            } else {
                println!("Metadata will be skipped at the end to match video length\n");
            }

            true
        } else {
            false
        };

        println!("Rewriting file with interleaved HDR10+ SEI NALs..");
        stdout().flush().ok();

        self.progress_bar = initialize_progress_bar(&IoFormat::Raw, &self.input)?;

        let chunk_size = 100_000;

        let mut processor =
            HevcProcessor::new(IoFormat::Raw, HevcProcessorOpts::default(), chunk_size);

        let file = File::open(&self.input)?;
        let mut reader = Box::new(BufReader::with_capacity(chunk_size, file));

        processor.process_io(&mut reader, self)
    }

    fn get_metadata_and_index_to_insert(
        frames: &[Frame],
        metadata_list: &[Hdr10PlusJsonMetadata],
        frame_buffer: &FrameBuffer,
        mismatched_length: bool,
        last_metadata: &Option<NalBuffer>,
        validate: bool,
    ) -> Result<(usize, NalBuffer)> {
        let existing_frame = frames
            .iter()
            .find(|f| f.decoded_number == frame_buffer.frame_number);

        // If we have a metadata buffered frame, write it
        // Otherwise, write the same data as previous
        let hdr10plus_nb = if let Some(frame) = existing_frame {
            if let Some(ref mut meta) = metadata_list.get(frame.presentation_number as usize) {
                let hdr10plus_data = hdr10plus::hevc::encode_hevc_from_json(meta, validate)?;

                Some(NalBuffer {
                    nal_type: NAL_SEI_PREFIX,
                    start_code: NALUStartCode::Length4,
                    data: hdr10plus_data,
                })
            } else if mismatched_length {
                last_metadata.clone()
            } else {
                bail!(
                    "No metadata found for presentation frame {}",
                    frame.presentation_number
                );
            }
        } else if mismatched_length {
            last_metadata.clone()
        } else {
            None
        };

        if let Some(hdr10plus_nb) = hdr10plus_nb {
            // First slice
            let insert_index = frame_buffer
                .nals
                .iter()
                .position(|nb| NALUnit::is_type_slice(nb.nal_type));

            if let Some(idx) = insert_index {
                // we want the SEI before the slice
                Ok((idx, hdr10plus_nb))
            } else {
                bail!(
                    "No slice in decoded frame {}. Cannot insert HDR10+ SEI.",
                    frame_buffer.frame_number
                );
            }
        } else {
            bail!(
                "No HDR10+ SEI data to write for decoded frame {}",
                frame_buffer.frame_number
            );
        }
    }
}

impl Av1Injector {
    fn from_args(args: InjectArgs, cli_options: CliOptions) -> Result<Self> {
        let InjectArgs {
            input,
            input_pos,
            json,
            output,
        } = args;

        let input = input_from_either("inject", input, input_pos)?;

        let output = match output {
            Some(path) => path,
            None => PathBuf::from("injected_output.ivf"),
        };

        let chunk_size = 100_000;
        let progress_bar = initialize_progress_bar(&IoFormat::Raw, &input)?;
        let writer = BufWriter::with_capacity(chunk_size, File::create(output)?);

        println!("Parsing JSON file...");
        stdout().flush().ok();

        let metadata_root = MetadataJsonRoot::from_file(&json)?;
        let metadata_list = metadata_root.scene_info;

        if metadata_list.is_empty() {
            bail!("Empty HDR10+ SceneInfo array");
        }

        Ok(Self {
            input,
            options: cli_options,
            metadata_list,
            progress_bar,
            writer,
            mismatched_length: false,
        })
    }

    fn read_ivf_header(reader: &mut BufReader<File>) -> Result<IvfHeader> {
        let mut header = [0u8; 32];
        reader.read_exact(&mut header)?;

        if &header[0..4] != b"DKIF" {
            bail!("Invalid IVF header magic");
        }

        if &header[8..12] != b"AV01" {
            bail!("IVF is not AV1 fourcc");
        }

        let frame_count = u32::from_le_bytes(header[24..28].try_into().unwrap());
        let timebase_den = u32::from_le_bytes(header[16..20].try_into().unwrap());
        let timebase_num = u32::from_le_bytes(header[20..24].try_into().unwrap());

        Ok(IvfHeader {
            raw: header,
            frame_count,
            timebase_den,
            timebase_num,
        })
    }

    fn metadata_for_frame(
        &mut self,
        frame_index: usize,
        last_obu: &Option<Vec<u8>>,
    ) -> Result<Option<Vec<u8>>> {
        if let Some(meta) = self.metadata_list.get(frame_index) {
            let obu = encode_av1_from_json(meta, self.options.validate)?;
            Ok(Some(obu))
        } else if self.mismatched_length {
            Ok(last_obu.clone())
        } else {
            self.mismatched_length = true;

            if let Some(previous) = last_obu {
                println!(
                    "\nWarning: mismatched lengths. video has more frames than metadata (first missing: {}).\nMetadata will be duplicated at the end to match video length\n",
                    frame_index
                );

                Ok(Some(previous.clone()))
            } else {
                bail!("No metadata found for frame {}", frame_index)
            }
        }
    }

    fn inject_ivf(&mut self) -> Result<()> {
        println!("Processing AV1 IVF input...");
        stdout().flush().ok();

        let chunk_size = 100_000;

        let mut reader = BufReader::with_capacity(chunk_size, File::open(&self.input)?);
        let header = Self::read_ivf_header(&mut reader)?;
        self.writer.write_all(&header.raw)?;

        self.mismatched_length =
            if header.frame_count as usize != self.metadata_list.len() && header.frame_count != 0 {
                println!(
                    "\nWarning: mismatched lengths. video {}, HDR10+ JSON {}",
                    header.frame_count,
                    self.metadata_list.len()
                );

                if self.metadata_list.len() < header.frame_count as usize {
                    println!("Metadata will be duplicated at the end to match video length\n");
                } else {
                    println!("Metadata will be skipped at the end to match video length\n");
                }

                true
            } else {
                false
            };

        let mut last_obu: Option<Vec<u8>> = None;
        let mut frame_index: usize = 0;
        let mut processed_bytes: u64 = 32;
        let mut actual_frames: u32 = 0;
        let timestamp_step: u64 = if header.timebase_num == 0 {
            if header.timebase_den != 0 {
                println!(
                    "Warning: IVF timebase numerator is 0; defaulting to 1 tick per frame (denominator = {})",
                    header.timebase_den
                );
            }

            1
        } else {
            header.timebase_num as u64
        };
        let mut current_timestamp: u64 = 0;

        loop {
            let mut frame_header = [0u8; 12];
            if reader.read_exact(&mut frame_header).is_err() {
                break;
            }

            let frame_size = u32::from_le_bytes(frame_header[0..4].try_into().unwrap()) as usize;

            let mut frame_buf = vec![0u8; frame_size];
            reader.read_exact(&mut frame_buf)?;

            let metadata = self.metadata_for_frame(frame_index, &last_obu)?;
            if let Some(ref obu) = metadata {
                last_obu = Some(obu.clone());
            }

            let mut out_frame =
                Vec::with_capacity(frame_size + metadata.as_ref().map_or(0, |m| m.len()));
            if let Some(obu) = metadata {
                out_frame.extend_from_slice(&obu);
            }
            out_frame.extend_from_slice(&frame_buf);

            ensure!(
                out_frame.len() <= u32::MAX as usize,
                "Frame too large after injection"
            );

            self.writer
                .write_all(&(out_frame.len() as u32).to_le_bytes())?;
            self.writer.write_all(&current_timestamp.to_le_bytes())?;
            self.writer.write_all(&out_frame)?;

            frame_index += 1;
            actual_frames += 1;
            current_timestamp = current_timestamp.saturating_add(timestamp_step);

            processed_bytes += 12 + frame_size as u64;
            self.progress_bar
                .set_position(processed_bytes / 100_000_000);

        }

        self.writer.flush()?;
        if header.frame_count == 0 || header.frame_count != actual_frames {
            self.writer.get_mut().seek(SeekFrom::Start(24))?;
            self.writer
                .get_mut()
                .write_all(&actual_frames.to_le_bytes())?;
            self.writer.flush()?;

            if header.frame_count != 0 && header.frame_count != actual_frames {
                println!(
                    "\nWarning: IVF header frame count {} replaced with {} to match actual frames\n",
                    header.frame_count, actual_frames
                );
            }
        }
        self.progress_bar.finish_and_clear();

        Ok(())
    }
}

impl IoProcessor for Injector {
    fn input(&self) -> &PathBuf {
        &self.input
    }

    fn update_progress(&mut self, delta: u64) {
        if !self.already_checked_for_hdr10plus {
            self.already_checked_for_hdr10plus = true;
        }

        self.progress_bar.inc(delta);
    }

    fn process_nals(&mut self, _parser: &HevcParser, nals: &[NALUnit], chunk: &[u8]) -> Result<()> {
        // Second pass
        if !self.frames.is_empty() && !self.nals.is_empty() {
            let metadata_list = &self.metadata_list;

            for nal in nals {
                if self.frame_buffer.frame_number != nal.decoded_frame_index {
                    let (idx, hdr10plus_nb) = Self::get_metadata_and_index_to_insert(
                        &self.frames,
                        metadata_list,
                        &self.frame_buffer,
                        self.mismatched_length,
                        &self.last_metadata_written,
                        self.options.validate,
                    )?;

                    self.last_metadata_written = Some(hdr10plus_nb.clone());
                    self.frame_buffer.nals.insert(idx, hdr10plus_nb);

                    // Write NALUs for the frame
                    for nal_buf in &self.frame_buffer.nals {
                        self.writer.write_all(NALUStartCode::Length4.slice())?;
                        self.writer.write_all(&nal_buf.data)?;
                    }

                    self.frame_buffer.frame_number = nal.decoded_frame_index;
                    self.frame_buffer.nals.clear();
                }

                let (st2094_40_msg, payload) = if nal.nal_type == NAL_SEI_PREFIX {
                    let sei_payload =
                        clear_start_code_emulation_prevention_3_byte(&chunk[nal.start..nal.end]);
                    let msg = st2094_40_sei_msg(&sei_payload, self.options.validate)?;

                    (msg, Some(sei_payload))
                } else {
                    (None, None)
                };

                if let (Some(msg), Some(mut payload)) = (st2094_40_msg, payload) {
                    let messages = SeiMessage::parse_sei_rbsp(&payload)?;

                    // Only remove ST2094-40 message if there are others
                    if messages.len() > 1 {
                        let start = msg.msg_offset;
                        let end = msg.payload_offset + msg.payload_size;

                        payload.drain(start..end);
                        add_start_code_emulation_prevention_3_byte(&mut payload);

                        self.frame_buffer.nals.push(NalBuffer {
                            nal_type: nal.nal_type,
                            start_code: nal.start_code,
                            data: payload,
                        });
                    }
                } else {
                    self.frame_buffer.nals.push(NalBuffer {
                        nal_type: nal.nal_type,
                        start_code: nal.start_code,
                        data: chunk[nal.start..nal.end].to_vec(),
                    });
                }
            }
        } else if !self.already_checked_for_hdr10plus {
            let existing_hdr10plus = nals
                .iter()
                .filter(|nal| nal.nal_type == NAL_SEI_PREFIX)
                .any(|nal| {
                    let sei_payload =
                        clear_start_code_emulation_prevention_3_byte(&chunk[nal.start..nal.end]);

                    st2094_40_sei_msg(&sei_payload, self.options.validate)
                        .unwrap_or(None)
                        .is_some()
                });

            if existing_hdr10plus {
                self.already_checked_for_hdr10plus = true;
                println!("\nWarning: Input file already has HDR10+ SEIs, they will be replaced.");
            }
        }

        Ok(())
    }

    fn finalize(&mut self, parser: &HevcParser) -> Result<()> {
        // First pass
        if self.frames.is_empty() && self.nals.is_empty() {
            self.frames.clone_from(parser.ordered_frames());
            self.nals.clone_from(parser.get_nals());
        } else {
            let ordered_frames = parser.ordered_frames();
            let total_frames = ordered_frames.len();

            // Last slice wasn't considered (no AUD/EOS NALU at the end)
            if (self.frame_buffer.frame_number as usize) != total_frames
                && !self.frame_buffer.nals.is_empty()
            {
                let metadata_list = &self.metadata_list;

                let (idx, hdr10plus_nb) = Self::get_metadata_and_index_to_insert(
                    &self.frames,
                    metadata_list,
                    &self.frame_buffer,
                    self.mismatched_length,
                    &self.last_metadata_written,
                    self.options.validate,
                )?;

                self.last_metadata_written = Some(hdr10plus_nb.clone());
                self.frame_buffer.nals.insert(idx, hdr10plus_nb);

                // Write NALUs for the last frame
                for nal_buf in &self.frame_buffer.nals {
                    self.writer.write_all(NALUStartCode::Length4.slice())?;
                    self.writer.write_all(&nal_buf.data)?;
                }

                self.frame_buffer.nals.clear();
            }

            // Second pass
            self.writer.flush()?;
        }

        self.progress_bar.finish_and_clear();

        Ok(())
    }
}
