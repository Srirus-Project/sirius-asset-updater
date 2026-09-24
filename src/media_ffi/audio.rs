//! The audio half of the FFmpeg bridge: PCM encoding, resampling and the
//! sample FIFO an encoder with a fixed frame size needs.

use std::ffi::c_void;

use rsmpeg::ffi;

use super::MediaError;

use super::error::{check, ffmpeg_error, media_error};
use super::raii::{ChannelLayout, Frame, SwrContext};
// The codec choice, the loader guard and the encoder drain live with the
// transcode drivers; audio encoding calls into them rather than the reverse.
use super::drain_encoder;

pub(super) unsafe fn resample_audio_frame(
    encoder_ctx: *mut ffi::AVCodecContext,
    decoded: *mut ffi::AVFrame,
    converted: *mut ffi::AVFrame,
) -> Result<*mut ffi::AVFrame, MediaError> {
    unsafe {
        (*converted).format = (*encoder_ctx).sample_fmt;
        (*converted).sample_rate = (*encoder_ctx).sample_rate;
        (*converted).nb_samples = (*decoded).nb_samples;
        check(
            ffi::av_channel_layout_copy(&mut (*converted).ch_layout, &(*encoder_ctx).ch_layout),
            "av_channel_layout_copy audio resample output",
        )?;
        check(
            ffi::av_frame_get_buffer(converted, 0),
            "av_frame_get_buffer audio resample",
        )?;

        if (*decoded).ch_layout.order == ffi::AV_CHANNEL_ORDER_UNSPEC {
            let channels = (*decoded).ch_layout.nb_channels;
            ffi::av_channel_layout_uninit(&mut (*decoded).ch_layout);
            ffi::av_channel_layout_default(&mut (*decoded).ch_layout, channels);
        }
        let input_layout = ChannelLayout::default_or_copy(&(*decoded).ch_layout)?;
        let swr = SwrContext::new(
            &(*converted).ch_layout,
            (*encoder_ctx).sample_fmt,
            (*encoder_ctx).sample_rate,
            &input_layout.inner,
            (*decoded).format,
            (*decoded).sample_rate,
        )?;
        check(
            ffi::swr_convert_frame(swr.ptr, converted, decoded),
            "swr_convert_frame",
        )?;
        Ok(converted)
    }
}

pub(super) unsafe fn choose_sample_format(
    codec: *const ffi::AVCodec,
    decoder_format: ffi::AVSampleFormat,
) -> Result<ffi::AVSampleFormat, MediaError> {
    unsafe {
        if (*codec).sample_fmts.is_null() {
            return Ok(decoder_format);
        }
        let mut cursor = (*codec).sample_fmts;
        while *cursor != ffi::AV_SAMPLE_FMT_NONE {
            if *cursor == decoder_format {
                return Ok(decoder_format);
            }
            cursor = cursor.add(1);
        }
        Ok(*(*codec).sample_fmts)
    }
}

// Not an owning wrapper: this drives the encoder through
// `drain_encoder` as it drains, so it stays with the transcoding logic
// rather than moving to `raii` with the types that only own memory.
pub(super) struct AudioFifo {
    ptr: *mut ffi::AVAudioFifo,
    frame_size: i32,
    pad_final_frame: bool,
    sample_fmt: ffi::AVSampleFormat,
    sample_rate: i32,
    ch_layout: ChannelLayout,
    frame: Frame,
}

impl AudioFifo {
    pub(super) fn new(encoder_ctx: *mut ffi::AVCodecContext) -> Result<Option<Self>, MediaError> {
        unsafe {
            if (*encoder_ctx).codec_type != ffi::AVMEDIA_TYPE_AUDIO
                || (*encoder_ctx).frame_size <= 0
            {
                return Ok(None);
            }
            let ch_layout = ChannelLayout::default_or_copy(&(*encoder_ctx).ch_layout)?;
            let ptr = ffi::av_audio_fifo_alloc(
                (*encoder_ctx).sample_fmt,
                (*encoder_ctx).ch_layout.nb_channels,
                (*encoder_ctx).frame_size,
            );
            if ptr.is_null() {
                return Err(media_error("av_audio_fifo_alloc failed"));
            }
            Ok(Some(Self {
                ptr,
                frame_size: (*encoder_ctx).frame_size,
                pad_final_frame: (*encoder_ctx).codec_id == ffi::AV_CODEC_ID_MP3,
                sample_fmt: (*encoder_ctx).sample_fmt,
                sample_rate: (*encoder_ctx).sample_rate,
                ch_layout,
                frame: Frame::new()?,
            }))
        }
    }

    pub(super) unsafe fn push(&mut self, frame: *mut ffi::AVFrame) -> Result<(), MediaError> {
        let samples = unsafe { (*frame).nb_samples };
        let written = unsafe {
            ffi::av_audio_fifo_write(
                self.ptr,
                (*frame).data.as_ptr() as *const *mut c_void,
                samples,
            )
        };
        if written == samples {
            Ok(())
        } else if written < 0 {
            Err(MediaError::Media {
                message: format!("av_audio_fifo_write failed: {}", ffmpeg_error(written)),
            })
        } else {
            Err(media_error(
                "av_audio_fifo_write wrote fewer samples than requested",
            ))
        }
    }

    pub(super) unsafe fn encode_available(
        &mut self,
        encoder_ctx: *mut ffi::AVCodecContext,
        output_ctx: *mut ffi::AVFormatContext,
        output_stream: *mut ffi::AVStream,
        frame_index: &mut i64,
        flush: bool,
    ) -> Result<(), MediaError> {
        loop {
            let available = unsafe { ffi::av_audio_fifo_size(self.ptr) };
            if available <= 0 || (!flush && available < self.frame_size) {
                break;
            }
            let samples = self.samples_to_encode(available, flush);
            unsafe {
                self.fill_frame_from_fifo(available, samples)?;
                (*self.frame.ptr).pts = *frame_index;
                *frame_index += samples as i64;
                check(
                    ffi::avcodec_send_frame(encoder_ctx, self.frame.ptr),
                    "avcodec_send_frame",
                )?;
                drain_encoder(encoder_ctx, output_ctx, output_stream)?;
                ffi::av_frame_unref(self.frame.ptr);
            }
        }
        Ok(())
    }

    pub(super) fn samples_to_encode(&self, available: i32, flush: bool) -> i32 {
        if flush && self.pad_final_frame && available < self.frame_size {
            self.frame_size
        } else if flush {
            available.min(self.frame_size)
        } else {
            self.frame_size
        }
    }

    pub(super) unsafe fn fill_frame_from_fifo(
        &mut self,
        available: i32,
        samples: i32,
    ) -> Result<(), MediaError> {
        unsafe {
            ffi::av_frame_unref(self.frame.ptr);
            (*self.frame.ptr).format = self.sample_fmt;
            (*self.frame.ptr).sample_rate = self.sample_rate;
            (*self.frame.ptr).nb_samples = samples;
            check(
                ffi::av_channel_layout_copy(
                    &mut (*self.frame.ptr).ch_layout,
                    &self.ch_layout.inner,
                ),
                "av_channel_layout_copy audio fifo frame",
            )?;
            check(
                ffi::av_frame_get_buffer(self.frame.ptr, 0),
                "av_frame_get_buffer audio fifo frame",
            )?;
            let expected_read = available.min(samples);
            let read = ffi::av_audio_fifo_read(
                self.ptr,
                (*self.frame.ptr).data.as_ptr() as *const *mut c_void,
                expected_read,
            );
            if read != expected_read {
                return Err(audio_fifo_read_error(read));
            }
            if read < samples {
                check(
                    ffi::av_samples_set_silence(
                        (*self.frame.ptr).extended_data,
                        read,
                        samples - read,
                        self.ch_layout.inner.nb_channels,
                        self.sample_fmt,
                    ),
                    "av_samples_set_silence audio fifo padding",
                )?;
            }
        }
        Ok(())
    }
}

impl Drop for AudioFifo {
    fn drop(&mut self) {
        unsafe {
            ffi::av_audio_fifo_free(self.ptr);
        }
    }
}

pub(super) fn audio_fifo_read_error(read: i32) -> MediaError {
    if read < 0 {
        MediaError::Media {
            message: format!("av_audio_fifo_read failed: {}", ffmpeg_error(read)),
        }
    } else {
        media_error("av_audio_fifo_read read fewer samples than requested")
    }
}
