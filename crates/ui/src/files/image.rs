//! Read-only raster preview helpers.
//!
//! Workspace images reuse the conversation decoder (`Image::from_bytes`)
//! and its format table; this module only extracts intrinsic dimensions so
//! the preview can state them without decoding on the render path.

use gpui::ImageFormat;

/// Intrinsic pixel size for the formats the conversation decoder supports.
/// `None` means the header is truncated or malformed; the image itself may
/// still decode, so callers treat this as unknown rather than unsupported.
pub(crate) fn dimensions(format: ImageFormat, bytes: &[u8]) -> Option<(u32, u32)> {
    match format {
        ImageFormat::Png => png(bytes),
        ImageFormat::Jpeg => jpeg(bytes),
        ImageFormat::Gif => gif(bytes),
        ImageFormat::Webp => webp(bytes),
        ImageFormat::Bmp => bmp(bytes),
        ImageFormat::Svg | ImageFormat::Tiff | ImageFormat::Ico | ImageFormat::Pnm => None,
    }
}

fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn le_u16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn le_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn png(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") || bytes.get(12..16)? != b"IHDR" {
        return None;
    }
    let width = be_u32(bytes, 16)?;
    let height = be_u32(bytes, 20)?;
    (width > 0 && height > 0).then_some((width, height))
}

fn gif(bytes: &[u8]) -> Option<(u32, u32)> {
    if !(bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return None;
    }
    let width = u32::from(le_u16(bytes, 6)?);
    let height = u32::from(le_u16(bytes, 8)?);
    (width > 0 && height > 0).then_some((width, height))
}

fn bmp(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(b"BM") {
        return None;
    }
    let width = le_u32(bytes, 18)? as i32;
    let height = le_u32(bytes, 22)? as i32;
    let (width, height) = (width.unsigned_abs(), height.unsigned_abs());
    (width > 0 && height > 0).then_some((width, height))
}

fn jpeg(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    let mut at = 2usize;
    while at + 4 <= bytes.len() {
        if bytes[at] != 0xff {
            at += 1;
            continue;
        }
        let mut marker = bytes[at + 1];
        while marker == 0xff && at + 2 < bytes.len() {
            at += 1;
            marker = bytes[at + 1];
        }
        at += 2;
        if marker == 0xd8 || marker == 0xd9 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        let length = usize::from(be_u16(bytes, at)?);
        if length < 2 || at + length > bytes.len() {
            return None;
        }
        let is_sof = matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf);
        if is_sof {
            let height = u32::from(be_u16(bytes, at + 3)?);
            let width = u32::from(be_u16(bytes, at + 5)?);
            return (width > 0 && height > 0).then_some((width, height));
        }
        at += length;
    }
    None
}

fn be_u16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn webp(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 30 || !bytes.starts_with(b"RIFF") || bytes.get(8..12)? != b"WEBP" {
        return None;
    }
    match bytes.get(12..16)? {
        b"VP8X" => {
            let width = 1 + u32::from_le_bytes([bytes[24], bytes[25], bytes[26], 0]);
            let height = 1 + u32::from_le_bytes([bytes[27], bytes[28], bytes[29], 0]);
            Some((width, height))
        }
        b"VP8 " => {
            if bytes.get(23..26)? != [0x9d, 0x01, 0x2a] {
                return None;
            }
            let width = u32::from(le_u16(bytes, 26)? & 0x3fff);
            let height = u32::from(le_u16(bytes, 28)? & 0x3fff);
            (width > 0 && height > 0).then_some((width, height))
        }
        b"VP8L" => {
            if bytes.get(20) != Some(&0x2f) {
                return None;
            }
            let bits = le_u32(bytes, 21)?;
            let width = 1 + (bits & 0x3fff);
            let height = 1 + ((bits >> 14) & 0x3fff);
            (width > 0 && height > 0).then_some((width, height))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_png_and_gif_headers() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        assert_eq!(dimensions(ImageFormat::Png, &png), Some((640, 480)));

        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&320u16.to_le_bytes());
        gif.extend_from_slice(&200u16.to_le_bytes());
        assert_eq!(dimensions(ImageFormat::Gif, &gif), Some((320, 200)));
    }

    #[test]
    fn truncated_headers_are_unknown_not_a_panic() {
        assert_eq!(dimensions(ImageFormat::Png, b"\x89PNG"), None);
        assert_eq!(dimensions(ImageFormat::Jpeg, b"\xff\xd8\xff"), None);
        assert_eq!(dimensions(ImageFormat::Webp, b"RIFF"), None);
    }
}
