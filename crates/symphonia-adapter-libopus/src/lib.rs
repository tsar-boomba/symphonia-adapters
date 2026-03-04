#![warn(missing_docs, missing_debug_implementations)]
#![forbid(clippy::unwrap_used)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc = include_str!("../README.md")]
#![no_std]

#[macro_use]
extern crate alloc;

use core::fmt;

use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::vec::Vec;
use symphonia_core::async_trait;
use symphonia_core::audio::{
    AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, Channels, GenericAudioBufferRef,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_OPUS;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, unsupported_error};
use symphonia_core::io::{BufReader, ReadBytes};
use symphonia_core::packet::Packet;

use crate::decoder::Decoder;

mod decoder;

/// Maximum sampling rate is 48 kHz for normal opus, and 96 kHz for Opus HD in the 1.6 spec.
const MAX_SAMPLE_RATE: usize = 48000;
const DEFAULT_SAMPLE_RATE: usize = 48000;
/// Assuming 48 kHz sample rate with the default 20 ms frames.
const DEFAULT_SAMPLES_PER_CHANNEL: usize = DEFAULT_SAMPLE_RATE * 20 / 1000;
/// Opus maximum frame size is 60 ms, with worst case being 120 ms when combining frames per packet.
const MAX_SAMPLES_PER_CHANNEL: usize = MAX_SAMPLE_RATE * 120 / 1000;

/// Symphonia-compatible wrapper for the libopus decoder.
pub struct OpusDecoder {
    params: AudioCodecParameters,
    decoder: Decoder,
    buf: AudioBuffer<f32>,
    pcm: Vec<f32>,
    samples_per_channel: usize,
    sample_rate: u32,
    channels: Channels,
    pre_skip: usize,
}

impl fmt::Debug for OpusDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpusDecoder")
            .field("params", &self.params)
            .field("decoder", &self.decoder)
            .field("buf", &"<buf>")
            .field("pcm", &self.pcm)
            .field("samples_per_channel", &self.samples_per_channel)
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("pre_skip", &self.pre_skip)
            .finish()
    }
}

// This should probably be handled in the Ogg demuxer, but we'll include it here for now.
async fn parse_pre_skip(buf: &[u8]) -> Result<usize> {
    // See https://wiki.xiph.org/OggOpus

    let mut reader = BufReader::new(buf);

    // Header - "OpusHead"
    let mut header = [0; 8];
    reader.read_buf_exact(&mut header).await?;

    // Version - 1 is the only valid version currently
    reader.read_byte().await?;

    // Number of channels (same as what we get from the CodecParameters)
    reader.read_byte().await?;

    // Pre-skip - number of samples (at 48 kHz) to discard from the start of the stream
    let pre_skip = reader.read_u16().await?;

    Ok(pre_skip as usize)
}

#[async_trait]
impl RegisterableAudioDecoder for OpusDecoder {
    async fn try_registry_new(
        params: &AudioCodecParameters,
        _opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        let channels = if let Some(channels) = &params.channels {
            channels.clone()
        } else {
            return unsupported_error("opus: channels or channel layout is required");
        };
        let num_channels = channels.count();

        let sample_rate = if let Some(sample_rate) = params.sample_rate {
            sample_rate
        } else {
            return unsupported_error("opus: sample rate required");
        };

        if !(1..=2).contains(&num_channels) {
            return unsupported_error("opus: unsupported number of channels");
        }

        let pre_skip = if let Some(extra_data) = &params.extra_data {
            parse_pre_skip(extra_data).await.unwrap_or_default()
        } else {
            0
        };

        Ok(Box::new(Self {
            params: params.to_owned(),
            decoder: Decoder::new(sample_rate, num_channels as u32)?,
            buf: audio_buffer(sample_rate, DEFAULT_SAMPLES_PER_CHANNEL, channels.clone()),
            pcm: vec![0.0; MAX_SAMPLES_PER_CHANNEL * 2],
            samples_per_channel: DEFAULT_SAMPLES_PER_CHANNEL,
            sample_rate,
            channels,
            pre_skip,
        }))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec]
    where
        Self: Sized,
    {
        &[SupportedAudioCodec {
            id: CODEC_ID_OPUS,
            info: CodecInfo {
                long_name: "Opus",
                short_name: "opus",
                profiles: &[],
            },
        }]
    }
}

#[async_trait]
impl AudioDecoder for OpusDecoder {
    fn codec_info(&self) -> &CodecInfo {
        &CodecInfo {
            long_name: "Opus",
            short_name: "opus",
            profiles: &[],
        }
    }

    fn reset(&mut self) {
        self.decoder.reset()
    }

    fn codec_params(&self) -> &AudioCodecParameters {
        &self.params
    }

    async fn decode(&mut self, packet: &Packet) -> Result<GenericAudioBufferRef<'_>> {
        let samples_per_channel = self.decoder.decode(&packet.data, &mut self.pcm)?;

        if samples_per_channel > self.samples_per_channel {
            // If new frame had more samples, allocate new buffer
            self.buf = audio_buffer(self.sample_rate, samples_per_channel, self.channels.clone());
            self.samples_per_channel = samples_per_channel;
        }

        let samples = samples_per_channel * self.channels.count();
        let pcm = &self.pcm[..samples];

        self.buf.clear();
        self.buf.render_uninit(Some(samples_per_channel));
        match self.channels.count() {
            1 => {
                let Some(plane) = self.buf.plane_mut(0) else {
                    unreachable!()
                };

                plane.copy_from_slice(pcm);
            }
            2 => {
                let Some((l, r)) = self.buf.plane_pair_mut(0, 1) else {
                    unreachable!()
                };

                for (i, j) in (0..samples).step_by(2).enumerate() {
                    l[i] = pcm[j];
                    r[i] = pcm[j + 1];
                }
            }
            _ => {}
        }

        // Pre-skip should only be used for the first packet, after that it should always be 0.
        self.pre_skip = 0;
        Ok(self.buf.as_generic_audio_buffer_ref())
    }

    fn finalize(&mut self) -> FinalizeResult {
        FinalizeResult::default()
    }

    fn last_decoded(&self) -> GenericAudioBufferRef<'_> {
        self.buf.as_generic_audio_buffer_ref()
    }
}

fn audio_buffer(
    sample_rate: u32,
    samples_per_channel: usize,
    channels: Channels,
) -> AudioBuffer<f32> {
    let spec = AudioSpec::new(sample_rate, channels);
    AudioBuffer::new(spec, samples_per_channel)
}
