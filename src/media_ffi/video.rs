//! The video half: scaling frames and picking a pixel format.

use std::ptr;

use rsmpeg::ffi;

use super::MediaError;

use super::error::{check, media_error};

pub(super) unsafe fn scale_video_frame(
    decoder_ctx: *mut ffi::AVCodecContext,
    encoder_ctx: *mut ffi::AVCodecContext,
    decoded: *mut ffi::AVFrame,
    converted: *mut ffi::AVFrame,
) -> Result<*mut ffi::AVFrame, MediaError> {
    unsafe {
        (*converted).format = (*encoder_ctx).pix_fmt;
        (*converted).width = (*encoder_ctx).width;
        (*converted).height = (*encoder_ctx).height;
        check(
            ffi::av_frame_get_buffer(converted, 0),
            "av_frame_get_buffer",
        )?;
        let sws = ffi::sws_getContext(
            (*decoder_ctx).width,
            (*decoder_ctx).height,
            (*decoded).format,
            (*encoder_ctx).width,
            (*encoder_ctx).height,
            (*encoder_ctx).pix_fmt,
            ffi::SWS_BILINEAR as i32,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null(),
        );
        if sws.is_null() {
            return Err(media_error("sws_getContext failed"));
        }
        let rows = ffi::sws_scale(
            sws,
            (*decoded).data.as_ptr() as *const *const u8,
            (*decoded).linesize.as_ptr(),
            0,
            (*decoder_ctx).height,
            (*converted).data.as_mut_ptr(),
            (*converted).linesize.as_mut_ptr(),
        );
        ffi::sws_freeContext(sws);
        if rows != (*encoder_ctx).height {
            return Err(media_error("incomplete video pixel conversion"));
        }
        Ok(converted)
    }
}
