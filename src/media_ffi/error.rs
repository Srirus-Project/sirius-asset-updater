//! Turning FFmpeg return codes and C-string conversions into typed errors.
//!
//! A failed FFI call carries two things worth keeping: which operation
//! failed, and what FFmpeg said about it. Everything here exists to stop
//! either half from being dropped on the way up.

use std::ffi::{c_char, CStr, CString};
use std::path::Path;

use rsmpeg::ffi;

use super::MediaError;

pub(super) fn valid_rational(value: ffi::AVRational) -> Option<ffi::AVRational> {
    (value.num > 0 && value.den > 0).then_some(value)
}

pub(super) fn path_cstring(path: &Path) -> Result<CString, MediaError> {
    if !path.is_absolute() {
        return Err(media_error("FFmpeg paths must be absolute local paths"));
    }
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes()
    };
    #[cfg(not(unix))]
    let bytes = path
        .to_str()
        .ok_or_else(|| media_error("path is not valid Unicode"))?
        .as_bytes();
    CString::new(bytes).map_err(|_| media_error("path contains NUL byte"))
}

pub(super) fn cstring(value: &str) -> Result<CString, MediaError> {
    CString::new(value).map_err(|err| MediaError::Media {
        message: format!("FFmpeg string contains NUL byte: {err}"),
    })
}

pub(super) fn check(ret: i32, operation: &str) -> Result<(), MediaError> {
    super::control::check()?;
    if ret >= 0 {
        Ok(())
    } else {
        Err(MediaError::Media {
            message: format!("{operation} failed: {}", ffmpeg_error(ret)),
        })
    }
}

pub(super) fn ffmpeg_error(code: i32) -> String {
    let mut buf = [0 as c_char; 128];
    unsafe {
        if ffi::av_strerror(code, buf.as_mut_ptr(), buf.len()) == 0 {
            CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
        } else {
            format!("FFmpeg error {code}")
        }
    }
}

pub(super) fn media_error(message: &str) -> MediaError {
    MediaError::Media {
        message: message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `check` turns a negative FFmpeg return into a message that names both
    /// the operation and what FFmpeg said. Losing either half makes an FFI
    /// failure unattributable, which is the usual way these get debugged.
    #[test]
    fn check_reports_the_operation_and_the_ffmpeg_reason() {
        assert!(check(0, "noop").is_ok());
        assert!(check(1, "positive returns are success").is_ok());

        let err = check(super::super::AVERROR_EOF, "avcodec_receive_frame").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("avcodec_receive_frame"), "{message}");
        assert!(
            message.contains(&ffmpeg_error(super::super::AVERROR_EOF)),
            "the FFmpeg reason must survive: {message}"
        );
    }

    /// An unrecognised code still has to produce something, not an empty string
    /// or a panic.
    #[test]
    fn ffmpeg_error_always_produces_a_message() {
        assert!(!ffmpeg_error(super::super::AVERROR_EOF).is_empty());
        assert!(!ffmpeg_error(i32::MIN + 1).is_empty());
    }

    /// Paths and option strings reach FFmpeg as C strings, so an interior NUL
    /// has to be rejected here rather than silently truncating the value.
    #[test]
    fn c_string_conversion_rejects_interior_nul() {
        assert_eq!(cstring("libmp3lame").unwrap().to_bytes(), b"libmp3lame");
        assert!(cstring("bad\0name").is_err());

        assert!(path_cstring(&std::env::temp_dir().join("out.mp3")).is_ok());
        assert!(path_cstring(Path::new("/tmp/ba\0d.mp3")).is_err());
    }

    #[test]
    fn only_positive_rationals_are_valid() {
        let rational = |num, den| ffi::AVRational { num, den };
        assert!(valid_rational(rational(30, 1)).is_some());
        assert!(valid_rational(rational(0, 1)).is_none());
        assert!(valid_rational(rational(30, 0)).is_none());
        assert!(valid_rational(rational(-1, 1)).is_none());
    }
}
