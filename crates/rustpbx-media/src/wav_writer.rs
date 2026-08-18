use anyhow::Result;
use audio_codec::CodecType;
use std::path::Path;
use tokio::fs::File;
use tokio::io::{AsyncSeekExt, AsyncWriteExt, SeekFrom};

pub(crate) struct CodecWavWriter {
    file: File,
    sample_rate: u32,
    channels: u16,
    codec: Option<CodecType>,
    written_bytes: u32,
}

impl CodecWavWriter {
    pub fn new(file: File, sample_rate: u32, channels: u16, codec: Option<CodecType>) -> Self {
        Self {
            file,
            sample_rate,
            channels,
            codec,
            written_bytes: 0,
        }
    }
    pub async fn create(
        path: &str,
        sample_rate: u32,
        channels: u16,
        codec: Option<CodecType>,
    ) -> Result<Self> {
        if let Some(parent) = Path::new(path).parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        let file = File::create(path)
            .await
            .map_err(|error| anyhow::anyhow!("Failed to create recorder file {path}: {error}"))?;
        let mut writer = Self::new(file, sample_rate, channels, codec);
        writer.write_header().await?;
        writer.file.flush().await?;
        Ok(writer)
    }

    async fn write_header(&mut self) -> Result<()> {
        let header = Self::wav_header(
            self.codec,
            self.sample_rate,
            self.channels,
            self.written_bytes,
        );
        self.file.write_all(&header).await?;
        Ok(())
    }

    pub async fn write_packet(&mut self, data: &[u8]) -> Result<()> {
        self.file.write_all(data).await?;
        self.written_bytes += data.len() as u32;
        Ok(())
    }

    pub async fn finalize(&mut self) -> Result<()> {
        self.file.seek(SeekFrom::Start(0)).await?;
        self.write_header().await?;
        self.file.flush().await?;
        Ok(())
    }

    fn wav_header(
        codec: Option<CodecType>,
        sample_rate: u32,
        channels: u16,
        data_size: u32,
    ) -> [u8; 44] {
        rustpbx_record_common::wav_header(
            &rustpbx_record_common::WavSpec {
                codec,
                sample_rate,
                channels,
            },
            data_size,
        )
    }
}
