use std::{fs::File, path::Path};

use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};
use thiserror::Error;

pub mod av1_parser;

pub const TOOL_NAME: &str = env!("CARGO_PKG_NAME");
pub const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Error, Debug)]
pub enum ParserError {
    #[error("File doesn't contain dynamic metadata")]
    NoMetadataFound,
    #[error("Dynamic HDR10+ metadata detected.")]
    MetadataDetected,
}

pub fn initialize_progress_bar(input: &Path) -> Result<ProgressBar> {
    let file = File::open(input).expect("No file found");
    let file_meta = file.metadata()?;
    let bytes_count = file_meta.len() / 100_000_000;

    let pb = ProgressBar::new(bytes_count);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("[{elapsed_precise}] {bar:60.cyan} {percent}%")?,
    );

    Ok(pb)
}
