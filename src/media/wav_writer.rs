use crate::media::StreamWriter;
use anyhow::Result;
use audio_codec::CodecType;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};

pub struct WavWriter<W: Write + Seek> {
    writer: W,
    sample_rate: u32,
    channels: u16,
    codec: Option<CodecType>,
    written_bytes: u32,
}

impl<W: Write + Seek + Send + Sync> StreamWriter for WavWriter<W> {
    fn write_header(&mut self) -> Result<()> {
        self.write_header_internal()
    }

    fn write_packet(&mut self, data: &[u8], _samples: usize) -> Result<()> {
        self.write_packet_internal(data)
    }

    fn finalize(&mut self) -> Result<()> {
        self.finalize_internal()
    }
}

impl WavWriter<File> {
    pub fn new(file: File, sample_rate: u32, channels: u16, codec: Option<CodecType>) -> Self {
        Self {
            writer: file,
            sample_rate,
            channels,
            codec,
            written_bytes: 0,
        }
    }
}

impl<W: Write + Seek> WavWriter<W> {
    pub fn new_with_writer(
        writer: W,
        sample_rate: u32,
        channels: u16,
        codec: Option<CodecType>,
    ) -> Self {
        Self {
            writer,
            sample_rate,
            channels,
            codec,
            written_bytes: 0,
        }
    }

    pub fn write_header_internal(&mut self) -> Result<()> {
        Self::write_wav_header(
            &mut self.writer,
            self.codec,
            self.sample_rate,
            self.channels,
            self.written_bytes,
        )
    }

    pub fn write_packet_internal(&mut self, data: &[u8]) -> Result<()> {
        self.writer.write_all(data)?;
        self.written_bytes += data.len() as u32;
        Ok(())
    }

    pub fn finalize_internal(&mut self) -> Result<()> {
        self.writer.seek(SeekFrom::Start(0))?;
        self.write_header_internal()
    }

    fn write_wav_header(
        file: &mut W,
        codec: Option<CodecType>,
        sample_rate: u32,
        channels: u16,
        data_size: u32,
    ) -> Result<()> {
        let mut header = [0u8; 44];
        header[0..4].copy_from_slice(b"RIFF");
        let file_size = 36 + data_size;
        header[4..8].copy_from_slice(&file_size.to_le_bytes());
        header[8..12].copy_from_slice(b"WAVE");
        header[12..16].copy_from_slice(b"fmt ");
        header[16..20].copy_from_slice(&16u32.to_le_bytes()); // fmt chunk size

        let format_tag: u16 = match codec {
            Some(CodecType::PCMU) => 7,      // mu-law
            Some(CodecType::PCMA) => 6,      // a-law
            Some(CodecType::G722) => 0x0065, // G.722
            Some(CodecType::G729) => 0x0083, // G.729
            None => 1,                       // PCM
            _ => 1,                          // Default to PCM
        };

        header[20..22].copy_from_slice(&format_tag.to_le_bytes());
        header[22..24].copy_from_slice(&channels.to_le_bytes());
        header[24..28].copy_from_slice(&sample_rate.to_le_bytes());

        let (bits_per_sample, byte_rate, block_align) = match codec {
            Some(CodecType::PCMU) | Some(CodecType::PCMA) => {
                let bps = 8;
                let br = sample_rate * channels as u32 * (bps as u32 / 8);
                let ba = channels * (bps / 8);
                (bps, br, ba)
            }
            Some(CodecType::G722) => {
                let bps = 8;
                let br = sample_rate * channels as u32 / 2; // G.722 is 4 bits per sample
                let ba = channels; // 1 byte per channel
                (bps, br, ba)
            }
            Some(CodecType::G729) => (0u16, 1000 * channels as u32, 10 * channels),
            _ => {
                let bps = 16;
                let br = sample_rate * channels as u32 * (bps as u32 / 8);
                let ba = channels * (bps / 8);
                (bps, br, ba)
            }
        };

        header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
        header[32..34].copy_from_slice(&block_align.to_le_bytes());
        header[34..36].copy_from_slice(&bits_per_sample.to_le_bytes());
        header[36..40].copy_from_slice(b"data");
        header[40..44].copy_from_slice(&data_size.to_le_bytes());

        file.write_all(&header)?;
        Ok(())
    }
}
