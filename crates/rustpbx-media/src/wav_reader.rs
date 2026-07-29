use anyhow::{Result, anyhow};
use audio_codec::{CodecType, Decoder, create_decoder};
use std::fmt;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

// ── Format tags ─────────────────────────────────────────────────────────

pub const FORMAT_PCM: u16 = 0x0001;
pub const FORMAT_IEEE_FLOAT: u16 = 0x0003;
pub const FORMAT_PCMA: u16 = 0x0006;
pub const FORMAT_PCMU: u16 = 0x0007;
pub const FORMAT_G722: u16 = 0x0065;
pub const FORMAT_G729: u16 = 0x0083;
pub const FORMAT_EXTENSIBLE: u16 = 0xFFFE;

// ── SampleFormat ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    Int,
    Float,
}

// ── WavSpec ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WavSpec {
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
    pub sample_format: SampleFormat,
}

impl WavSpec {
    pub fn byte_rate(&self) -> u32 {
        self.sample_rate * self.channels as u32 * (self.bits_per_sample as u32 / 8)
    }

    pub fn block_align(&self) -> u16 {
        self.channels * (self.bits_per_sample / 8)
    }

    fn format_tag(&self) -> u16 {
        match self.sample_format {
            SampleFormat::Float => FORMAT_IEEE_FLOAT,
            SampleFormat::Int => FORMAT_PCM,
        }
    }
}

// ── WavFormat (detected from file) ──────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WavFormat {
    Pcm,
    IeeeFloat,
    Pcma,
    Pcmu,
    G722,
    G729,
    Extensible,
    Unknown(u16),
}

impl WavFormat {
    pub fn from_format_tag(tag: u16) -> Self {
        match tag {
            FORMAT_PCM => WavFormat::Pcm,
            FORMAT_IEEE_FLOAT => WavFormat::IeeeFloat,
            FORMAT_PCMA => WavFormat::Pcma,
            FORMAT_PCMU => WavFormat::Pcmu,
            FORMAT_G722 => WavFormat::G722,
            FORMAT_G729 => WavFormat::G729,
            FORMAT_EXTENSIBLE => WavFormat::Extensible,
            _ => WavFormat::Unknown(tag),
        }
    }

    pub fn format_tag(self) -> u16 {
        match self {
            WavFormat::Pcm => FORMAT_PCM,
            WavFormat::IeeeFloat => FORMAT_IEEE_FLOAT,
            WavFormat::Pcma => FORMAT_PCMA,
            WavFormat::Pcmu => FORMAT_PCMU,
            WavFormat::G722 => FORMAT_G722,
            WavFormat::G729 => FORMAT_G729,
            WavFormat::Extensible => FORMAT_EXTENSIBLE,
            WavFormat::Unknown(tag) => tag,
        }
    }

    pub fn needs_decode(self) -> bool {
        matches!(
            self,
            WavFormat::Pcma | WavFormat::Pcmu | WavFormat::G722 | WavFormat::G729
        )
    }

    pub fn codec_type(self) -> Option<CodecType> {
        match self {
            WavFormat::Pcma => Some(CodecType::PCMA),
            WavFormat::Pcmu => Some(CodecType::PCMU),
            WavFormat::G722 => Some(CodecType::G722),
            WavFormat::G729 => Some(CodecType::G729),
            _ => None,
        }
    }

    pub fn frame_size(self) -> usize {
        match self {
            WavFormat::Pcma | WavFormat::Pcmu => 160,
            WavFormat::G722 => 160,
            WavFormat::G729 => 10,
            _ => 1,
        }
    }
}

// ── WavReader ───────────────────────────────────────────────────────────

pub struct WavReader<R: Read> {
    reader: R,
    spec: WavSpec,
    #[allow(dead_code)]
    data_offset: u64,
    data_len: u32,
    bytes_read: u32,
    format: WavFormat,
    decoder: Option<Box<dyn Decoder>>,
    decode_frame_size: usize,
    decoded_buffer: Vec<i16>,
    decoded_pos: usize,
}

impl<R: Read> fmt::Debug for WavReader<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WavReader")
            .field("spec", &self.spec)
            .field("format", &self.format)
            .field("data_len", &self.data_len)
            .field("bytes_read", &self.bytes_read)
            .finish()
    }
}

impl WavReader<BufReader<File>> {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let file = File::open(path.as_ref()).map_err(|e| {
            anyhow!(
                "Failed to open WAV file '{}': {}",
                path.as_ref().display(),
                e
            )
        })?;
        let reader = BufReader::new(file);
        Self::new(reader)
    }
}

impl<R: Read + Seek> WavReader<R> {
    pub fn new(mut reader: R) -> Result<Self> {
        let mut header_buf = [0u8; 12];
        reader.read_exact(&mut header_buf)?;

        if &header_buf[0..4] != b"RIFF" {
            return Err(anyhow!("Not a WAV file: missing RIFF tag"));
        }
        if &header_buf[8..12] != b"WAVE" {
            return Err(anyhow!("Not a WAV file: missing WAVE tag"));
        }

        let mut format_tag: u16 = 0;
        let mut channels: u16 = 0;
        let mut sample_rate: u32 = 0;
        let mut _byte_rate: u32 = 0;
        let mut _block_align: u16 = 0;
        let mut bits_per_sample: u16 = 0;
        let mut _data_offset: u64 = 0;
        let mut data_len: u32 = 0;
        let mut fmt_found = false;
        let mut data_found = false;

        let mut chunk_buf = [0u8; 4];

        loop {
            if reader.read_exact(&mut chunk_buf).is_err() {
                break;
            }
            let chunk_id = &chunk_buf;

            let mut len_buf = [0u8; 4];
            if reader.read_exact(&mut len_buf).is_err() {
                break;
            }
            let chunk_len = u32::from_le_bytes(len_buf);

            if chunk_id == b"fmt " {
                if chunk_len < 16 {
                    return Err(anyhow!("Invalid fmt chunk size: {}", chunk_len));
                }

                let mut fmt_buf = [0u8; 16];
                reader.read_exact(&mut fmt_buf)?;

                format_tag = u16::from_le_bytes([fmt_buf[0], fmt_buf[1]]);
                channels = u16::from_le_bytes([fmt_buf[2], fmt_buf[3]]);
                sample_rate = u32::from_le_bytes([fmt_buf[4], fmt_buf[5], fmt_buf[6], fmt_buf[7]]);
                _byte_rate = u32::from_le_bytes([fmt_buf[8], fmt_buf[9], fmt_buf[10], fmt_buf[11]]);
                _block_align = u16::from_le_bytes([fmt_buf[12], fmt_buf[13]]);
                bits_per_sample = u16::from_le_bytes([fmt_buf[14], fmt_buf[15]]);

                fmt_found = true;

                let extra_bytes = chunk_len - 16;
                if extra_bytes > 0 {
                    reader.seek(SeekFrom::Current(extra_bytes as i64))?;
                }
            } else if chunk_id == b"data" {
                if !fmt_found {
                    return Err(anyhow!("data chunk found before fmt chunk"));
                }
                _data_offset = reader.stream_position()?;
                data_len = chunk_len;
                data_found = true;
                break;
            } else {
                if chunk_len > 0 {
                    reader.seek(SeekFrom::Current(chunk_len as i64))?;
                }
            }
        }

        if !fmt_found {
            return Err(anyhow!("No fmt chunk found in WAV file"));
        }
        if !data_found {
            return Err(anyhow!("No data chunk found in WAV file"));
        }

        let format = WavFormat::from_format_tag(format_tag);

        let sample_format = match format {
            WavFormat::IeeeFloat => SampleFormat::Float,
            _ => SampleFormat::Int,
        };

        let decoder = if format.needs_decode() {
            let codec = format.codec_type().unwrap();
            Some(create_decoder(codec))
        } else {
            None
        };

        let decode_frame_size = format.frame_size();

        Ok(Self {
            reader,
            spec: WavSpec {
                channels,
                sample_rate,
                bits_per_sample,
                sample_format,
            },
            data_offset: _data_offset,
            data_len,
            bytes_read: 0,
            format,
            decoder,
            decode_frame_size,
            decoded_buffer: Vec::new(),
            decoded_pos: 0,
        })
    }

    pub fn spec(&self) -> &WavSpec {
        &self.spec
    }

    pub fn duration(&self) -> u32 {
        let block_align = if self.spec.block_align() == 0 {
            1
        } else {
            self.spec.block_align()
        };
        self.data_len / block_align as u32
    }

    pub fn samples(&mut self) -> WavSamplesIter<'_, R> {
        WavSamplesIter { reader: self }
    }

    fn read_next_sample(&mut self) -> Option<Result<i16>> {
        if self.format.needs_decode() {
            self.read_decoded_sample()
        } else {
            match self.format {
                WavFormat::Pcm | WavFormat::Extensible | WavFormat::Unknown(_) => {
                    self.read_pcm_sample()
                }
                WavFormat::IeeeFloat => self.read_float_sample(),
                _ => None,
            }
        }
    }

    fn read_pcm_sample(&mut self) -> Option<Result<i16>> {
        match self.spec.bits_per_sample {
            16 => {
                let mut buf = [0u8; 2];
                if self.bytes_read + 2 > self.data_len {
                    return None;
                }
                match self.reader.read_exact(&mut buf) {
                    Ok(()) => {
                        self.bytes_read += 2;
                        Some(Ok(i16::from_le_bytes(buf)))
                    }
                    Err(_) => None,
                }
            }
            8 => {
                let mut buf = [0u8; 1];
                if self.bytes_read + 1 > self.data_len {
                    return None;
                }
                match self.reader.read_exact(&mut buf) {
                    Ok(()) => {
                        self.bytes_read += 1;
                        let val = (buf[0] as i16).wrapping_sub(128).wrapping_shl(8);
                        Some(Ok(val))
                    }
                    Err(_) => None,
                }
            }
            24 => {
                let mut buf = [0u8; 3];
                if self.bytes_read + 3 > self.data_len {
                    return None;
                }
                match self.reader.read_exact(&mut buf) {
                    Ok(()) => {
                        self.bytes_read += 3;
                        let val =
                            (buf[0] as i32) | ((buf[1] as i32) << 8) | ((buf[2] as i32) << 16);
                        let val = if val & 0x800000 != 0 {
                            val | 0xFF00_0000u32 as i32
                        } else {
                            val
                        };
                        Some(Ok((val >> 8) as i16))
                    }
                    Err(_) => None,
                }
            }
            32 => {
                let mut buf = [0u8; 4];
                if self.bytes_read + 4 > self.data_len {
                    return None;
                }
                match self.reader.read_exact(&mut buf) {
                    Ok(()) => {
                        self.bytes_read += 4;
                        let val = i32::from_le_bytes(buf);
                        Some(Ok((val >> 16) as i16))
                    }
                    Err(_) => None,
                }
            }
            _ => None,
        }
    }

    fn read_float_sample(&mut self) -> Option<Result<i16>> {
        match self.spec.bits_per_sample {
            32 => {
                let mut buf = [0u8; 4];
                if self.bytes_read + 4 > self.data_len {
                    return None;
                }
                match self.reader.read_exact(&mut buf) {
                    Ok(()) => {
                        self.bytes_read += 4;
                        let f = f32::from_le_bytes(buf);
                        let clamped = f.clamp(-1.0, 1.0);
                        Some(Ok((clamped * 32767.0) as i16))
                    }
                    Err(_) => None,
                }
            }
            64 => {
                let mut buf = [0u8; 8];
                if self.bytes_read + 8 > self.data_len {
                    return None;
                }
                match self.reader.read_exact(&mut buf) {
                    Ok(()) => {
                        self.bytes_read += 8;
                        let f = f64::from_le_bytes(buf);
                        let clamped = f.clamp(-1.0, 1.0);
                        Some(Ok((clamped * 32767.0) as i16))
                    }
                    Err(_) => None,
                }
            }
            _ => None,
        }
    }

    fn read_decoded_sample(&mut self) -> Option<Result<i16>> {
        if self.decoded_pos < self.decoded_buffer.len() {
            let sample = self.decoded_buffer[self.decoded_pos];
            self.decoded_pos += 1;
            return Some(Ok(sample));
        }

        let remaining = self.data_len - self.bytes_read;
        if remaining == 0 {
            return None;
        }

        let frame_size = self.decode_frame_size;
        let bytes_to_read = if remaining >= frame_size as u32 {
            frame_size
        } else {
            remaining as usize
        };

        let mut encoded_buf = vec![0u8; bytes_to_read];
        match self.reader.read_exact(&mut encoded_buf) {
            Ok(()) => {
                self.bytes_read += bytes_to_read as u32;
            }
            Err(_) => {
                return None;
            }
        }

        if bytes_to_read < frame_size && matches!(self.format, WavFormat::G729) {
            encoded_buf.resize(frame_size, 0);
        }

        if let Some(ref mut decoder) = self.decoder {
            let pcm = decoder.decode(&encoded_buf);
            self.decoded_buffer = pcm;
            self.decoded_pos = 0;

            if !self.decoded_buffer.is_empty() {
                let sample = self.decoded_buffer[self.decoded_pos];
                self.decoded_pos += 1;
                Some(Ok(sample))
            } else {
                None
            }
        } else {
            None
        }
    }
}

impl<R: Read> WavReader<R> {
    pub fn into_inner(self) -> R {
        self.reader
    }
}

pub struct WavSamplesIter<'a, R: Read> {
    reader: &'a mut WavReader<R>,
}

impl<R: Read + Seek> Iterator for WavSamplesIter<'_, R> {
    type Item = Result<i16>;

    fn next(&mut self) -> Option<Self::Item> {
        self.reader.read_next_sample()
    }
}

// ── WavWriter ───────────────────────────────────────────────────────────

pub struct WavWriter<W: Write + Seek> {
    writer: W,
    spec: WavSpec,
    data_len: u32,
}

impl<W: Write + Seek + fmt::Debug> fmt::Debug for WavWriter<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WavWriter")
            .field("spec", &self.spec)
            .field("data_len", &self.data_len)
            .finish()
    }
}

impl WavWriter<File> {
    pub fn create(path: impl AsRef<Path>, spec: WavSpec) -> Result<Self> {
        let file = File::create(path)?;
        let mut writer = Self {
            writer: file,
            spec,
            data_len: 0,
        };
        writer.write_header()?;
        Ok(writer)
    }
}

impl<W: Write + Seek> WavWriter<W> {
    pub fn new(writer: W, spec: WavSpec) -> Result<Self> {
        let mut w = Self {
            writer,
            spec,
            data_len: 0,
        };
        w.write_header()?;
        Ok(w)
    }

    pub fn spec(&self) -> &WavSpec {
        &self.spec
    }

    pub fn write_sample<S: IntoSample>(&mut self, sample: S) -> Result<()> {
        let bytes = sample.to_bytes(self.spec.bits_per_sample, self.spec.sample_format);
        self.writer.write_all(&bytes)?;
        self.data_len += bytes.len() as u32;
        Ok(())
    }

    pub fn finalize(&mut self) -> Result<()> {
        self.writer.seek(SeekFrom::Start(0))?;
        self.write_header()?;
        self.writer.flush()?;
        Ok(())
    }

    fn write_header(&mut self) -> std::io::Result<()> {
        let format_tag = self.spec.format_tag();
        let channels = self.spec.channels;
        let sample_rate = self.spec.sample_rate;
        let byte_rate = self.spec.byte_rate();
        let block_align = self.spec.block_align();
        let bits_per_sample = self.spec.bits_per_sample;

        let mut header = [0u8; 44];
        header[0..4].copy_from_slice(b"RIFF");
        let file_size = 36 + self.data_len;
        header[4..8].copy_from_slice(&file_size.to_le_bytes());
        header[8..12].copy_from_slice(b"WAVE");
        header[12..16].copy_from_slice(b"fmt ");
        header[16..20].copy_from_slice(&16u32.to_le_bytes());
        header[20..22].copy_from_slice(&format_tag.to_le_bytes());
        header[22..24].copy_from_slice(&channels.to_le_bytes());
        header[24..28].copy_from_slice(&sample_rate.to_le_bytes());
        header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
        header[32..34].copy_from_slice(&block_align.to_le_bytes());
        header[34..36].copy_from_slice(&bits_per_sample.to_le_bytes());
        header[36..40].copy_from_slice(b"data");
        header[40..44].copy_from_slice(&self.data_len.to_le_bytes());

        self.writer.write_all(&header)
    }
}

// ── IntoSample trait ────────────────────────────────────────────────────

pub trait IntoSample {
    fn to_bytes(self, bits_per_sample: u16, sample_format: SampleFormat) -> Vec<u8>;
}

impl IntoSample for i16 {
    fn to_bytes(self, bits_per_sample: u16, sample_format: SampleFormat) -> Vec<u8> {
        match (bits_per_sample, sample_format) {
            (16, SampleFormat::Int) => self.to_le_bytes().to_vec(),
            (8, SampleFormat::Int) => {
                let unsigned = ((self >> 8) + 128i16) as u8;
                vec![unsigned]
            }
            (24, SampleFormat::Int) => {
                let val = (self as i32) << 8;
                vec![
                    (val & 0xFF) as u8,
                    ((val >> 8) & 0xFF) as u8,
                    ((val >> 16) & 0xFF) as u8,
                ]
            }
            (32, SampleFormat::Int) => {
                let val = (self as i32) << 16;
                val.to_le_bytes().to_vec()
            }
            (32, SampleFormat::Float) => {
                let f = self as f32 / 32768.0;
                f.to_le_bytes().to_vec()
            }
            (64, SampleFormat::Float) => {
                let f = self as f64 / 32768.0;
                f.to_le_bytes().to_vec()
            }
            _ => self.to_le_bytes().to_vec(),
        }
    }
}

impl IntoSample for f32 {
    fn to_bytes(self, bits_per_sample: u16, sample_format: SampleFormat) -> Vec<u8> {
        match (bits_per_sample, sample_format) {
            (32, SampleFormat::Float) => self.to_le_bytes().to_vec(),
            (16, SampleFormat::Int) => {
                let val = (self.clamp(-1.0, 1.0) * 32767.0) as i16;
                val.to_le_bytes().to_vec()
            }
            _ => self.to_le_bytes().to_vec(),
        }
    }
}

impl IntoSample for f64 {
    fn to_bytes(self, bits_per_sample: u16, sample_format: SampleFormat) -> Vec<u8> {
        match (bits_per_sample, sample_format) {
            (64, SampleFormat::Float) => self.to_le_bytes().to_vec(),
            (32, SampleFormat::Float) => (self as f32).to_le_bytes().to_vec(),
            (16, SampleFormat::Int) => {
                let val = (self.clamp(-1.0, 1.0) * 32767.0) as i16;
                val.to_le_bytes().to_vec()
            }
            _ => self.to_le_bytes().to_vec(),
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // ── helpers ─────────────────────────────────────────────────────────

    struct WavBuilder {
        format_tag: u16,
        channels: u16,
        sample_rate: u32,
        bits_per_sample: u16,
        data: Vec<u8>,
        extra_chunks: Vec<(Vec<u8>, Vec<u8>)>,
        fmt_extra: Vec<u8>,
        no_data: bool,
        no_fmt: bool,
        swap_fmt_data: bool,
    }

    impl WavBuilder {
        fn new(format_tag: u16, sample_rate: u32, channels: u16, bits_per_sample: u16) -> Self {
            Self {
                format_tag,
                channels,
                sample_rate,
                bits_per_sample,
                data: Vec::new(),
                extra_chunks: Vec::new(),
                fmt_extra: Vec::new(),
                no_data: false,
                no_fmt: false,
                swap_fmt_data: false,
            }
        }

        fn data(mut self, data: &[u8]) -> Self {
            self.data = data.to_vec();
            self
        }

        fn extra_chunk(mut self, id: &[u8], payload: &[u8]) -> Self {
            self.extra_chunks.push((id.to_vec(), payload.to_vec()));
            self
        }

        fn fmt_extra(mut self, extra: &[u8]) -> Self {
            self.fmt_extra = extra.to_vec();
            self
        }

        fn no_data(mut self) -> Self {
            self.no_data = true;
            self
        }

        fn no_fmt(mut self) -> Self {
            self.no_fmt = true;
            self
        }

        fn swap_fmt_data(mut self) -> Self {
            self.swap_fmt_data = true;
            self
        }

        fn build(self) -> Vec<u8> {
            let byte_rate =
                self.sample_rate * self.channels as u32 * (self.bits_per_sample as u32 / 8);
            let block_align = self.channels * (self.bits_per_sample / 8);

            let fmt_chunk_len = 16u32 + self.fmt_extra.len() as u32;
            let fmt_chunk = {
                let mut buf = Vec::new();
                buf.extend_from_slice(b"fmt ");
                buf.extend_from_slice(&fmt_chunk_len.to_le_bytes());
                buf.extend_from_slice(&self.format_tag.to_le_bytes());
                buf.extend_from_slice(&self.channels.to_le_bytes());
                buf.extend_from_slice(&self.sample_rate.to_le_bytes());
                buf.extend_from_slice(&byte_rate.to_le_bytes());
                buf.extend_from_slice(&block_align.to_le_bytes());
                buf.extend_from_slice(&self.bits_per_sample.to_le_bytes());
                buf.extend_from_slice(&self.fmt_extra);
                buf
            };

            let data_chunk = if !self.no_data {
                let mut buf = Vec::new();
                buf.extend_from_slice(b"data");
                buf.extend_from_slice(&(self.data.len() as u32).to_le_bytes());
                buf.extend_from_slice(&self.data);
                buf
            } else {
                Vec::new()
            };

            let mut extra_buf = Vec::new();
            for (id, payload) in &self.extra_chunks {
                extra_buf.extend_from_slice(id);
                extra_buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                extra_buf.extend_from_slice(payload);
            }

            let file_size =
                36 + self.data.len() as u32 + self.fmt_extra.len() as u32 + extra_buf.len() as u32;

            let mut wav = Vec::new();
            wav.extend_from_slice(b"RIFF");
            wav.extend_from_slice(&file_size.to_le_bytes());
            wav.extend_from_slice(b"WAVE");

            if self.swap_fmt_data {
                if !data_chunk.is_empty() {
                    wav.extend_from_slice(&data_chunk);
                }
                if !self.no_fmt {
                    wav.extend_from_slice(&fmt_chunk);
                }
            } else {
                if !self.no_fmt {
                    wav.extend_from_slice(&fmt_chunk);
                }
                wav.extend_from_slice(&extra_buf);
                if !data_chunk.is_empty() {
                    wav.extend_from_slice(&data_chunk);
                }
            }

            wav
        }
    }

    fn write_wav_file(data: &[u8]) -> tempfile::NamedTempFile {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".wav").expect("tempfile");
        tmp.write_all(data).expect("write wav");
        tmp.flush().expect("flush");
        tmp
    }

    fn write_read_roundtrip(
        samples: &[i16],
        sample_rate: u32,
        channels: u16,
        bits: u16,
    ) -> Vec<i16> {
        let spec = WavSpec {
            channels,
            sample_rate,
            bits_per_sample: bits,
            sample_format: SampleFormat::Int,
        };
        let mut buf = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut buf);
            let mut writer = WavWriter::new(cursor, spec).unwrap();
            for &s in samples {
                writer.write_sample(s).unwrap();
            }
            writer.finalize().unwrap();
        }
        let mut reader = WavReader::new(std::io::Cursor::new(&buf)).unwrap();
        reader.samples().map(|s| s.unwrap()).collect()
    }

    // ── WavWriter + WavReader roundtrip (PCM 16-bit) ───────────────────

    #[test]
    fn test_writer_reader_roundtrip_mono() {
        let samples: Vec<i16> = (0..160).collect();
        let read = write_read_roundtrip(&samples, 8000, 1, 16);
        assert_eq!(read, samples);
    }

    #[test]
    fn test_writer_reader_roundtrip_stereo() {
        let samples: Vec<i16> = (0..320).collect();
        let read = write_read_roundtrip(&samples, 44100, 2, 16);
        assert_eq!(read, samples);
    }

    #[test]
    fn test_writer_create_file_roundtrip() {
        let dir = std::env::temp_dir();
        let path = dir.join("wav_reader_test_create.wav");
        let samples: Vec<i16> = (0..100).collect();

        let spec = WavSpec {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        {
            let mut writer = WavWriter::create(&path, spec).unwrap();
            for &s in &samples {
                writer.write_sample(s).unwrap();
            }
            writer.finalize().unwrap();
        }

        let mut reader = WavReader::open(&path).unwrap();
        let read: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(read, samples);

        let _ = std::fs::remove_file(&path);
    }

    // ── PCM 16-bit ─────────────────────────────────────────────────────

    #[test]
    fn test_pcm_16bit_mono() {
        let samples: Vec<i16> = (0..160).collect();
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let wav = WavBuilder::new(FORMAT_PCM, 8000, 1, 16).data(&data).build();
        let mut reader = WavReader::new(Cursor::new(wav)).expect("open");

        assert_eq!(reader.spec().format_tag(), FORMAT_PCM);
        assert_eq!(reader.spec().channels, 1);
        assert_eq!(reader.spec().sample_rate, 8000);
        assert_eq!(reader.spec().bits_per_sample, 16);
        assert_eq!(reader.duration(), 160);

        let read: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(read, samples);
    }

    #[test]
    fn test_pcm_16bit_stereo() {
        let samples: Vec<i16> = (0..320).collect();
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let wav = WavBuilder::new(FORMAT_PCM, 44100, 2, 16)
            .data(&data)
            .build();
        let mut reader = WavReader::new(Cursor::new(wav)).expect("open");

        assert_eq!(reader.spec().channels, 2);
        assert_eq!(reader.spec().sample_rate, 44100);

        let read: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(read, samples);
    }

    // ── PCM 8-bit ──────────────────────────────────────────────────────

    #[test]
    fn test_pcm_8bit_roundtrip() {
        let samples: Vec<i16> = vec![0, 4096, -4096, 32767, -32768, 8192];
        let spec = WavSpec {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 8,
            sample_format: SampleFormat::Int,
        };
        let mut buf = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut buf);
            let mut writer = WavWriter::new(cursor, spec).unwrap();
            for &s in &samples {
                writer.write_sample(s).unwrap();
            }
            writer.finalize().unwrap();
        }
        let mut reader = WavReader::new(std::io::Cursor::new(&buf)).unwrap();
        assert_eq!(reader.spec().bits_per_sample, 8);
        let read: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(read.len(), samples.len());
        // 8-bit has limited precision, check each sample is within ±512
        for (orig, decoded) in samples.iter().zip(read.iter()) {
            let diff = (orig - decoded).unsigned_abs();
            assert!(
                diff <= 512,
                "8-bit roundtrip: orig={}, got={}, diff={}",
                orig,
                decoded,
                diff
            );
        }
    }

    // ── PCM 24-bit ─────────────────────────────────────────────────────

    #[test]
    fn test_pcm_24bit() {
        let mut data = Vec::new();
        for i in 0i32..10 {
            let val = i * 1000;
            data.push((val & 0xFF) as u8);
            data.push(((val >> 8) & 0xFF) as u8);
            data.push(((val >> 16) & 0xFF) as u8);
        }
        let wav = WavBuilder::new(FORMAT_PCM, 8000, 1, 24).data(&data).build();
        let mut reader = WavReader::new(Cursor::new(wav)).expect("open");
        let read: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(read.len(), 10);
        assert_eq!(read[0], 0);
    }

    // ── PCM 32-bit ─────────────────────────────────────────────────────

    #[test]
    fn test_pcm_32bit_roundtrip() {
        let samples: Vec<i16> = vec![0, 1000, -1000, 32767, -32768];
        let spec = WavSpec {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 32,
            sample_format: SampleFormat::Int,
        };
        let mut buf = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut buf);
            let mut writer = WavWriter::new(cursor, spec).unwrap();
            for &s in &samples {
                writer.write_sample(s).unwrap();
            }
            writer.finalize().unwrap();
        }
        let mut reader = WavReader::new(std::io::Cursor::new(&buf)).unwrap();
        let read: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(read, samples);
    }

    // ── IEEE Float ─────────────────────────────────────────────────────

    #[test]
    fn test_ieee_float_32bit() {
        let floats: Vec<f32> = vec![0.0, 0.5, -0.5, 1.0, -1.0];
        let spec = WavSpec {
            channels: 1,
            sample_rate: 44100,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        let mut buf = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut buf);
            let mut writer = WavWriter::new(cursor, spec).unwrap();
            for &f in &floats {
                writer.write_sample(f).unwrap();
            }
            writer.finalize().unwrap();
        }
        let mut reader = WavReader::new(std::io::Cursor::new(&buf)).unwrap();
        assert_eq!(reader.spec().sample_format, SampleFormat::Float);
        let read: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(read.len(), 5);
        assert_eq!(read[0], 0);
        assert!(read[1] > 16000 && read[1] < 17000);
        assert!(read[2] > -17000 && read[2] < -16000);
    }

    #[test]
    fn test_ieee_float_64bit() {
        let floats: Vec<f64> = vec![0.0, 1.0, -1.0];
        let spec = WavSpec {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 64,
            sample_format: SampleFormat::Float,
        };
        let mut buf = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut buf);
            let mut writer = WavWriter::new(cursor, spec).unwrap();
            for &f in &floats {
                writer.write_sample(f).unwrap();
            }
            writer.finalize().unwrap();
        }
        let mut reader = WavReader::new(std::io::Cursor::new(&buf)).unwrap();
        let read: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(read.len(), 3);
        assert_eq!(read[0], 0);
        assert_eq!(read[1], 32767);
        assert_eq!(read[2], -32767);
    }

    // ── PCMU ───────────────────────────────────────────────────────────

    #[test]
    fn test_pcmu_wav_decode() {
        let mut encoder = audio_codec::create_encoder(CodecType::PCMU);
        let pcm: Vec<i16> = vec![0, 1000, -1000, 32767, -32768, 5000];
        let encoded: Vec<u8> = encoder.encode(&pcm);

        let wav = WavBuilder::new(FORMAT_PCMU, 8000, 1, 8)
            .data(&encoded)
            .build();
        let mut reader = WavReader::new(Cursor::new(wav)).expect("open");

        let read: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(read.len(), pcm.len());
        for (orig, decoded) in pcm.iter().zip(read.iter()) {
            let diff = (orig - decoded).unsigned_abs();
            assert!(
                diff <= 1000,
                "PCMU: expected ~{}, got {}, diff={}",
                orig,
                decoded,
                diff
            );
        }
    }

    #[test]
    fn test_pcmu_large() {
        let pcm: Vec<i16> = (0..8000)
            .map(|i| ((i as f32 * 2.0 * 3.14159 / 8000.0).sin() * 16000.0) as i16)
            .collect();
        let mut encoder = audio_codec::create_encoder(CodecType::PCMU);
        let encoded: Vec<u8> = encoder.encode(&pcm);

        let wav = WavBuilder::new(FORMAT_PCMU, 8000, 1, 8)
            .data(&encoded)
            .build();
        let mut reader = WavReader::new(Cursor::new(wav)).expect("open");

        let read: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(read.len(), 8000);
    }

    // ── PCMA ───────────────────────────────────────────────────────────

    #[test]
    fn test_pcma_wav_decode() {
        let mut encoder = audio_codec::create_encoder(CodecType::PCMA);
        let pcm: Vec<i16> = vec![0, 1000, -1000, 32767, -32768, 5000];
        let encoded: Vec<u8> = encoder.encode(&pcm);

        let wav = WavBuilder::new(FORMAT_PCMA, 8000, 1, 8)
            .data(&encoded)
            .build();
        let mut reader = WavReader::new(Cursor::new(wav)).expect("open");

        let read: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(read.len(), pcm.len());
        for (orig, decoded) in pcm.iter().zip(read.iter()) {
            let diff = (orig - decoded).unsigned_abs();
            assert!(
                diff <= 1000,
                "PCMA: expected ~{}, got {}, diff={}",
                orig,
                decoded,
                diff
            );
        }
    }

    #[test]
    fn test_pcma_large() {
        let pcm: Vec<i16> = (0..8000)
            .map(|i| ((i as f32 * 2.0 * 3.14159 / 8000.0).sin() * 16000.0) as i16)
            .collect();
        let mut encoder = audio_codec::create_encoder(CodecType::PCMA);
        let encoded: Vec<u8> = encoder.encode(&pcm);

        let wav = WavBuilder::new(FORMAT_PCMA, 8000, 1, 8)
            .data(&encoded)
            .build();
        let mut reader = WavReader::new(Cursor::new(wav)).expect("open");

        let read: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(read.len(), 8000);
    }

    // ── G.722 ──────────────────────────────────────────────────────────

    #[test]
    fn test_g722_single_frame() {
        let mut encoder = audio_codec::create_encoder(CodecType::G722);
        let pcm: Vec<i16> = (0..160)
            .map(|i| ((i as f32 * 0.1).sin() * 8000.0) as i16)
            .collect();
        let encoded: Vec<u8> = encoder.encode(&pcm);

        eprintln!(
            "G722 encoded {} bytes from {} samples",
            encoded.len(),
            pcm.len()
        );

        let wav = WavBuilder::new(FORMAT_G722, 8000, 1, 8)
            .data(&encoded)
            .build();
        let mut reader = WavReader::new(Cursor::new(wav)).expect("open");

        let read: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
        eprintln!("G722 decoded {} samples", read.len());

        assert!(read.len() > 0);
        assert!(read.iter().any(|&s| s != 0));
    }

    #[test]
    fn test_g722_multiple_frames() {
        let mut encoder = audio_codec::create_encoder(CodecType::G722);
        let pcm: Vec<i16> = (0..480)
            .map(|i| ((i as f32 * 0.05).sin() * 8000.0) as i16)
            .collect();
        let encoded: Vec<u8> = encoder.encode(&pcm);

        let wav = WavBuilder::new(FORMAT_G722, 8000, 1, 8)
            .data(&encoded)
            .build();
        let mut reader = WavReader::new(Cursor::new(wav)).expect("open");

        let read: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
        assert!(read.len() > 0);
    }

    // ── G.729 ──────────────────────────────────────────────────────────

    #[test]
    fn test_g729_single_frame() {
        let mut encoder = audio_codec::create_encoder(CodecType::G729);
        let pcm: Vec<i16> = (0..80)
            .map(|i| ((i as f32 * 0.1).sin() * 4000.0) as i16)
            .collect();
        let encoded: Vec<u8> = encoder.encode(&pcm);

        assert_eq!(encoded.len(), 10);

        let wav = WavBuilder::new(FORMAT_G729, 8000, 1, 0)
            .data(&encoded)
            .build();
        let mut reader = WavReader::new(Cursor::new(wav)).expect("open");

        let read: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(read.len(), 80);
    }

    #[test]
    fn test_g729_multiple_frames() {
        let mut encoder = audio_codec::create_encoder(CodecType::G729);
        let pcm: Vec<i16> = (0..240)
            .map(|i| ((i as f32 * 0.05).sin() * 4000.0) as i16)
            .collect();
        let encoded: Vec<u8> = encoder.encode(&pcm);

        assert_eq!(encoded.len(), 30);

        let wav = WavBuilder::new(FORMAT_G729, 8000, 1, 0)
            .data(&encoded)
            .build();
        let mut reader = WavReader::new(Cursor::new(wav)).expect("open");

        let read: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(read.len(), 240);
    }

    #[test]
    fn test_g729_partial_frame_padded() {
        let partial = vec![0xABu8, 0xCD, 0xEF, 0x01, 0x23];
        let wav = WavBuilder::new(FORMAT_G729, 8000, 1, 0)
            .data(&partial)
            .build();
        let mut reader = WavReader::new(Cursor::new(wav)).expect("open");

        let read: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(read.len(), 80);
    }

    // ── Round-trip encode/decode ───────────────────────────────────────

    #[test]
    fn test_roundtrip_pcmu() {
        let original: Vec<i16> = (0..800)
            .map(|i| ((i as f32 * 2.0 * 3.14159 / 200.0).sin() * 10000.0) as i16)
            .collect();
        let mut encoder = audio_codec::create_encoder(CodecType::PCMU);
        let encoded: Vec<u8> = encoder.encode(&original);

        let wav = WavBuilder::new(FORMAT_PCMU, 8000, 1, 8)
            .data(&encoded)
            .build();
        let mut reader = WavReader::new(Cursor::new(wav)).expect("open");
        let decoded: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();

        assert_eq!(decoded.len(), original.len());
        for (i, (o, d)) in original.iter().zip(decoded.iter()).enumerate() {
            let diff = (o - d).unsigned_abs();
            assert!(
                diff <= 500,
                "roundtrip PCMU[{}]: expected ~{}, got {}, diff={}",
                i,
                o,
                d,
                diff
            );
        }
    }

    #[test]
    fn test_roundtrip_pcma() {
        let original: Vec<i16> = (0..800)
            .map(|i| ((i as f32 * 2.0 * 3.14159 / 200.0).sin() * 10000.0) as i16)
            .collect();
        let mut encoder = audio_codec::create_encoder(CodecType::PCMA);
        let encoded: Vec<u8> = encoder.encode(&original);

        let wav = WavBuilder::new(FORMAT_PCMA, 8000, 1, 8)
            .data(&encoded)
            .build();
        let mut reader = WavReader::new(Cursor::new(wav)).expect("open");
        let decoded: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();

        assert_eq!(decoded.len(), original.len());
        for (i, (o, d)) in original.iter().zip(decoded.iter()).enumerate() {
            let diff = (o - d).unsigned_abs();
            assert!(
                diff <= 500,
                "roundtrip PCMA[{}]: expected ~{}, got {}, diff={}",
                i,
                o,
                d,
                diff
            );
        }
    }

    // ── Chunk structure ────────────────────────────────────────────────

    #[test]
    fn test_junk_chunk_skipped() {
        let samples: Vec<i16> = (0..10).collect();
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let wav = WavBuilder::new(FORMAT_PCM, 8000, 1, 16)
            .data(&data)
            .extra_chunk(b"JUNK", &[0u8; 64])
            .build();

        let mut reader = WavReader::new(Cursor::new(wav)).expect("open");
        let read: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(read, samples);
    }

    #[test]
    fn test_list_and_fact_chunks() {
        let samples: Vec<i16> = (0..10).collect();
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let wav = WavBuilder::new(FORMAT_PCM, 8000, 1, 16)
            .data(&data)
            .extra_chunk(b"fact", &10u32.to_le_bytes())
            .extra_chunk(b"LIST", b"INFO test")
            .build();

        let mut reader = WavReader::new(Cursor::new(wav)).expect("open");
        let read: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(read, samples);
    }

    #[test]
    fn test_fmt_extra_bytes() {
        let samples: Vec<i16> = vec![42];
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let wav = WavBuilder::new(FORMAT_PCM, 8000, 1, 16)
            .data(&data)
            .fmt_extra(&[0u8; 4])
            .build();

        let mut reader = WavReader::new(Cursor::new(wav)).expect("open");
        let read: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(read, samples);
    }

    // ── Error cases ────────────────────────────────────────────────────

    #[test]
    fn test_missing_riff_tag() {
        let data = b"NOOP\x00\x00\x00\x00WAVEfmt ".to_vec();
        let result = WavReader::new(Cursor::new(data));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("RIFF"));
    }

    #[test]
    fn test_missing_wave_tag() {
        let data = b"RIFF\x00\x00\x00\x00NOOP".to_vec();
        let result = WavReader::new(Cursor::new(data));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("WAVE"));
    }

    #[test]
    fn test_no_fmt_chunk() {
        let wav = WavBuilder::new(FORMAT_PCM, 8000, 1, 16).no_fmt().build();
        let result = WavReader::new(Cursor::new(wav));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("fmt"));
    }

    #[test]
    fn test_no_data_chunk() {
        let wav = WavBuilder::new(FORMAT_PCM, 8000, 1, 16).no_data().build();
        let result = WavReader::new(Cursor::new(wav));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("data"));
    }

    #[test]
    fn test_data_before_fmt() {
        let samples: Vec<i16> = vec![42];
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let wav = WavBuilder::new(FORMAT_PCM, 8000, 1, 16)
            .data(&data)
            .swap_fmt_data()
            .build();
        let result = WavReader::new(Cursor::new(wav));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("before fmt"));
    }

    #[test]
    fn test_empty_data() {
        let wav = WavBuilder::new(FORMAT_PCM, 8000, 1, 16).data(&[]).build();
        let mut reader = WavReader::new(Cursor::new(wav)).expect("open");
        let read: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
        assert!(read.is_empty());
    }

    // ── Duration ───────────────────────────────────────────────────────

    #[test]
    fn test_duration_pcm() {
        let samples: Vec<i16> = vec![0i16; 8000];
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let wav = WavBuilder::new(FORMAT_PCM, 8000, 1, 16).data(&data).build();
        let reader = WavReader::new(Cursor::new(wav)).unwrap();
        assert_eq!(reader.duration(), 8000);
    }

    #[test]
    fn test_duration_pcmu() {
        let wav = WavBuilder::new(FORMAT_PCMU, 8000, 1, 8)
            .data(&vec![0u8; 8000])
            .build();
        let reader = WavReader::new(Cursor::new(wav)).unwrap();
        assert_eq!(reader.duration(), 8000);
    }

    // ── File open ──────────────────────────────────────────────────────

    #[test]
    fn test_open_from_file() {
        let samples: Vec<i16> = (0..160).collect();
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let wav = WavBuilder::new(FORMAT_PCM, 8000, 1, 16).data(&data).build();
        let tmp = write_wav_file(&wav);

        let mut reader = WavReader::open(tmp.path()).unwrap();
        let read: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(read, samples);
    }

    #[test]
    fn test_open_missing_file() {
        let result = WavReader::open("/nonexistent/path/test.wav");
        assert!(result.is_err());
    }

    // ── Partial read ───────────────────────────────────────────────────

    #[test]
    fn test_partial_read() {
        let samples: Vec<i16> = (0..160).collect();
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let wav = WavBuilder::new(FORMAT_PCM, 8000, 1, 16).data(&data).build();
        let mut reader = WavReader::new(Cursor::new(wav)).unwrap();

        let mut partial = Vec::new();
        for _ in 0..80 {
            partial.push(reader.samples().next().unwrap().unwrap());
        }
        assert_eq!(partial, samples[..80].to_vec());

        let rest: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(rest, samples[80..].to_vec());
    }

    // ── Single sample ──────────────────────────────────────────────────

    #[test]
    fn test_single_sample() {
        let data = 12345i16.to_le_bytes().to_vec();
        let wav = WavBuilder::new(FORMAT_PCM, 8000, 1, 16).data(&data).build();
        let mut reader = WavReader::new(Cursor::new(wav)).unwrap();
        let read: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
        assert_eq!(read, vec![12345i16]);
    }

    // ── WavFormat helpers ──────────────────────────────────────────────

    #[test]
    fn test_wav_format_roundtrip_tag() {
        for tag in [
            0x0001u16, 0x0003, 0x0006, 0x0007, 0x0065, 0x0083, 0xFFFE, 0x1234,
        ] {
            assert_eq!(WavFormat::from_format_tag(tag).format_tag(), tag);
        }
    }

    #[test]
    fn test_wav_spec_byte_rate() {
        let spec = WavSpec {
            channels: 2,
            sample_rate: 44100,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        assert_eq!(spec.byte_rate(), 44100 * 2 * 2);
        assert_eq!(spec.block_align(), 4);
    }

    #[test]
    fn test_wav_spec_format_tag() {
        let int_spec = WavSpec {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        assert_eq!(int_spec.format_tag(), 0x0001);

        let float_spec = WavSpec {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        assert_eq!(float_spec.format_tag(), 0x0003);
    }

    #[test]
    fn test_into_inner() {
        let data = 0i16.to_le_bytes().to_vec();
        let wav = WavBuilder::new(FORMAT_PCM, 8000, 1, 16).data(&data).build();
        let reader = WavReader::new(Cursor::new(wav)).unwrap();
        let cursor = reader.into_inner();
        assert!(cursor.into_inner().len() > 0);
    }
}
