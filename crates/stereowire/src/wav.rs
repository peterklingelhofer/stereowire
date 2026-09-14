//! Minimal WAV writer, used to record a received session.
//!
//! Audio arrives as uncompressed 32-bit float, so it is written out unchanged
//! rather than being quantised on the way to disk.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{Context, Result};
use stereowire_proto::packet::{SampleRate, AUDIO_CHANNELS};

/// IEEE float sample format.
const FORMAT_FLOAT: u16 = 3;
const BITS_PER_SAMPLE: u16 = 32;

pub struct WavWriter {
    file: BufWriter<File>,
    data_bytes: u32,
    rate: SampleRate,
}

impl WavWriter {
    pub fn create(path: &Path, rate: SampleRate) -> Result<Self> {
        let file =
            File::create(path).with_context(|| format!("could not create {}", path.display()))?;
        let mut writer = WavWriter {
            file: BufWriter::new(file),
            data_bytes: 0,
            rate,
        };
        writer.write_header()?;
        Ok(writer)
    }

    /// Writes a placeholder header; the sizes are filled in by `finish`.
    fn write_header(&mut self) -> Result<()> {
        let channels = AUDIO_CHANNELS as u16;
        let bytes_per_frame = u32::from(channels) * u32::from(BITS_PER_SAMPLE / 8);

        self.file.write_all(b"RIFF")?;
        self.file.write_all(&0u32.to_le_bytes())?;
        self.file.write_all(b"WAVE")?;

        self.file.write_all(b"fmt ")?;
        self.file.write_all(&16u32.to_le_bytes())?;
        self.file.write_all(&FORMAT_FLOAT.to_le_bytes())?;
        self.file.write_all(&channels.to_le_bytes())?;
        self.file.write_all(&self.rate.hz().to_le_bytes())?;
        self.file
            .write_all(&(self.rate.hz() * bytes_per_frame).to_le_bytes())?;
        self.file
            .write_all(&(bytes_per_frame as u16).to_le_bytes())?;
        self.file.write_all(&BITS_PER_SAMPLE.to_le_bytes())?;

        self.file.write_all(b"data")?;
        self.file.write_all(&0u32.to_le_bytes())?;
        Ok(())
    }

    pub fn write(&mut self, samples: &[f32]) -> Result<()> {
        for sample in samples {
            self.file.write_all(&sample.to_le_bytes())?;
        }
        self.data_bytes += (samples.len() * 4) as u32;
        Ok(())
    }

    /// Backfills the RIFF and data chunk sizes.
    pub fn finish(mut self) -> Result<()> {
        self.file.flush()?;
        let file = self.file.get_mut();
        file.seek(SeekFrom::Start(4))?;
        file.write_all(&(36 + self.data_bytes).to_le_bytes())?;
        file.seek(SeekFrom::Start(40))?;
        file.write_all(&self.data_bytes.to_le_bytes())?;
        file.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_u32(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
    }

    fn read_u16(bytes: &[u8], at: usize) -> u16 {
        u16::from_le_bytes(bytes[at..at + 2].try_into().expect("two bytes"))
    }

    #[test]
    fn writes_a_header_players_can_actually_read() {
        let path = std::env::temp_dir().join("stereowire-wav-header-test.wav");
        let mut wav = WavWriter::create(&path, SampleRate::Hz48000).expect("creates");
        let samples: Vec<f32> = (0..480).map(|i| (i as f32 / 480.0) - 0.5).collect();
        wav.write(&samples).expect("writes");
        wav.finish().expect("finalizes");

        let bytes = std::fs::read(&path).expect("reads back");
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        assert_eq!(&bytes[36..40], b"data");

        assert_eq!(read_u16(&bytes, 20), FORMAT_FLOAT, "IEEE float");
        assert_eq!(read_u16(&bytes, 22), AUDIO_CHANNELS as u16);
        assert_eq!(read_u32(&bytes, 24), 48_000);
        assert_eq!(read_u16(&bytes, 34), BITS_PER_SAMPLE);

        // The sizes are backfilled by `finish`; getting these wrong is the
        // classic way to produce a file that looks fine but will not open.
        let data_bytes = (samples.len() * 4) as u32;
        assert_eq!(read_u32(&bytes, 40), data_bytes, "data chunk size");
        assert_eq!(read_u32(&bytes, 4), 36 + data_bytes, "RIFF size");
        assert_eq!(bytes.len(), 44 + data_bytes as usize);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn samples_survive_the_round_trip_unchanged() {
        let path = std::env::temp_dir().join("stereowire-wav-samples-test.wav");
        let mut wav = WavWriter::create(&path, SampleRate::Hz48000).expect("creates");
        // Values chosen to catch byte-order and truncation mistakes.
        let samples = vec![0.0f32, 1.0, -1.0, 0.5, -0.25, f32::MIN_POSITIVE];
        wav.write(&samples).expect("writes");
        wav.finish().expect("finalizes");

        let bytes = std::fs::read(&path).expect("reads back");
        let (frames, rest) = bytes[44..].as_chunks::<4>();
        assert!(
            rest.is_empty(),
            "the payload must be a whole number of samples"
        );
        let decoded: Vec<f32> = frames.iter().copied().map(f32::from_le_bytes).collect();
        assert_eq!(decoded, samples, "audio must be stored losslessly");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn the_header_records_the_streams_actual_rate() {
        for rate in [
            SampleRate::Hz44100,
            SampleRate::Hz48000,
            SampleRate::Hz96000,
        ] {
            let path = std::env::temp_dir().join(format!("stereowire-wav-{}.wav", rate.hz()));
            let mut wav = WavWriter::create(&path, rate).expect("creates");
            wav.write(&[0.0; 8]).expect("writes");
            wav.finish().expect("finalizes");
            let bytes = std::fs::read(&path).expect("reads back");
            assert_eq!(read_u32(&bytes, 24), rate.hz(), "sample rate field");
            let bytes_per_frame = AUDIO_CHANNELS as u32 * u32::from(BITS_PER_SAMPLE / 8);
            assert_eq!(
                read_u32(&bytes, 28),
                rate.hz() * bytes_per_frame,
                "byte rate must agree, or players resample"
            );
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn an_empty_recording_is_still_a_valid_file() {
        let path = std::env::temp_dir().join("stereowire-wav-empty-test.wav");
        WavWriter::create(&path, SampleRate::Hz48000)
            .expect("creates")
            .finish()
            .expect("finalizes");
        let bytes = std::fs::read(&path).expect("reads back");
        assert_eq!(bytes.len(), 44, "header only");
        assert_eq!(read_u32(&bytes, 40), 0);
        assert_eq!(read_u32(&bytes, 4), 36);
        std::fs::remove_file(&path).ok();
    }
}
