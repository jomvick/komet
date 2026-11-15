//! The ASCII background effect: a source image is reduced to a grid of cells,
//! each cell picks a glyph from a ramp by luminance, and the glyphs are tinted
//! with the cell colour. The result is one RGBA texture, so GPUI paints a single
//! image per frame instead of thousands of text runs.
//!
//! Everything here is pure: no globals, no gpui `App`, no IO beyond `load_source`.

use std::path::{Path, PathBuf};

use font8x8::{BASIC_FONTS, BLOCK_FONTS, UnicodeFonts};
use image::{Rgba, RgbaImage};
use sha2::{Digest, Sha256};

use super::{AsciiCharset, AsciiColor, AsciiParams};

/// Glyph ramp for a charset, darkest → brightest.
pub fn ramp(charset: AsciiCharset) -> &'static str {
    match charset {
        AsciiCharset::Classic => " .:-=+*#%@",
        AsciiCharset::Blocks => " ░▒▓█",
        AsciiCharset::Binary => " 01",
    }
}

/// Pick the ramp glyph for `luma` (0..=1). `invert` flips the brightness first.
pub fn cell_char(luma: f32, invert: bool, ramp: &str) -> char {
    let luma = if invert { 1.0 - luma } else { luma };
    let last = ramp.chars().count().saturating_sub(1);
    let index = (luma.clamp(0.0, 1.0) * last as f32).round() as usize;
    ramp.chars().nth(index.min(last)).unwrap_or(' ')
}

/// The ASCII decoder only knows PNG and JPEG (the `image` features Komet builds).
pub fn supports_ascii(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            ext.eq_ignore_ascii_case("png")
                || ext.eq_ignore_ascii_case("jpg")
                || ext.eq_ignore_ascii_case("jpeg")
        })
}

/// `(cell_w, cell_h, rows)` for a target pixel size. Cells are 1:2 (a character
/// is taller than it is wide).
pub fn grid_size(target: (u32, u32), columns: u16) -> (u32, u32, u32) {
    let columns = u32::from(columns.max(1));
    let cell_w = (target.0 / columns).max(1);
    let cell_h = cell_w * 2;
    let rows = (target.1 / cell_h).max(1);
    (cell_w, cell_h, rows)
}

/// Average-free cell sample: the source pixel at the cell's centre, with the
/// same `Cover` crop GPUI applies when it paints the texture.
pub fn cell_at(
    src: &RgbaImage,
    cell: (u32, u32),
    grid: (u32, u32, u32),
    target: (u32, u32),
) -> (u8, u8, u8) {
    if src.width() == 0 || src.height() == 0 {
        return (0, 0, 0);
    }
    let (cell_w, cell_h, _) = grid;
    let target_w = target.0.max(1) as f32;
    let target_h = target.1.max(1) as f32;
    let scale = (src.width() as f32 / target_w).max(src.height() as f32 / target_h);
    let offset_x = (src.width() as f32 - target_w * scale) / 2.0;
    let offset_y = (src.height() as f32 - target_h * scale) / 2.0;

    let sample_x = ((cell.0 * cell_w + cell_w / 2) as f32 * scale + offset_x)
        .clamp(0.0, src.width() as f32 - 1.0);
    let sample_y = ((cell.1 * cell_h + cell_h / 2) as f32 * scale + offset_y)
        .clamp(0.0, src.height() as f32 - 1.0);

    let pixel = src.get_pixel(sample_x as u32, sample_y as u32);
    (pixel.0[0], pixel.0[1], pixel.0[2])
}

/// Rasterise the whole grid into one black-backed RGBA texture of `target` size.
pub fn render(
    src: &RgbaImage,
    params: &AsciiParams,
    target: (u32, u32),
    accent: [u8; 3],
) -> RgbaImage {
    let width = target.0.max(1);
    let height = target.1.max(1);
    let target = (width, height);
    let (cell_w, cell_h, rows) = grid_size(target, params.columns);
    let columns = u32::from(params.columns.max(1));
    let ramp = ramp(params.charset);
    let mut out = RgbaImage::from_pixel(width, height, Rgba([0, 0, 0, 255]));

    for cy in 0..rows {
        for cx in 0..columns {
            let (r, g, b) = cell_at(src, (cx, cy), (cell_w, cell_h, rows), target);
            let luma =
                (0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)) / 255.0;
            let glyph = cell_char(luma, params.invert, ramp);
            let Some(bitmap) = BASIC_FONTS.get(glyph).or_else(|| BLOCK_FONTS.get(glyph)) else {
                continue;
            };
            let color = match params.color {
                AsciiColor::Source => [r, g, b],
                AsciiColor::Mono => {
                    let gray = (luma * 255.0) as u8;
                    [gray, gray, gray]
                }
                AsciiColor::Accent => [
                    (f32::from(accent[0]) * luma) as u8,
                    (f32::from(accent[1]) * luma) as u8,
                    (f32::from(accent[2]) * luma) as u8,
                ],
            };

            for py in 0..cell_h {
                let bit_y = (py * 8 / cell_h) as usize;
                for px in 0..cell_w {
                    let bit_x = (px * 8 / cell_w) as usize;
                    if (bitmap[bit_y] >> bit_x) & 1 == 1 {
                        let x = cx * cell_w + px;
                        let y = cy * cell_h + py;
                        if x < width && y < height {
                            out.put_pixel(x, y, Rgba([color[0], color[1], color[2], 255]));
                        }
                    }
                }
            }
        }
    }
    out
}

/// Decode the source image, with a readable error for the UI/log.
pub fn load_source(path: &Path) -> Result<RgbaImage, String> {
    image::open(path)
        .map(|image| image.to_rgba8())
        .map_err(|error| format!("could not decode {}: {error}", path.display()))
}

/// Stable cache key: source path + every parameter + the 256 px size bucket.
pub fn cache_key(source: &Path, params: &AsciiParams, target: (u32, u32)) -> String {
    let bucket = |value: u32| (value / 256 * 256).max(256);
    let mut hasher = Sha256::new();
    hasher.update(source.to_string_lossy().as_bytes());
    hasher.update(
        format!(
            "|{}x{}|{}|{:?}|{:?}|{}|{:.3}",
            bucket(target.0),
            bucket(target.1),
            params.columns,
            params.charset,
            params.color,
            params.invert,
            params.opacity
        )
        .as_bytes(),
    );
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// At most one render per (image, params, size bucket); never queue a second.
pub fn should_render(ready_key: Option<&str>, pending: bool, wanted: &str) -> bool {
    ready_key != Some(wanted) && !pending
}

pub fn cache_path(data_dir: &Path, key: &str) -> PathBuf {
    data_dir.join("wallpaper-cache").join(format!("{key}.png"))
}

pub fn load_cached(cache: &Path) -> Option<RgbaImage> {
    image::open(cache).ok().map(|image| image.to_rgba8())
}

/// Write via a temp file + rename so a crash never leaves a half PNG.
pub fn store_cached(cache: &Path, image: &RgbaImage) {
    let Some(parent) = cache.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let tmp = cache.with_extension("png.tmp");
    if image
        .save_with_format(&tmp, image::ImageFormat::Png)
        .is_ok()
    {
        let _ = std::fs::rename(&tmp, cache);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luma_to_char_is_monotonic_and_inverted() {
        let r = ramp(AsciiCharset::Classic);
        assert_eq!(cell_char(0.0, false, r), ' ');
        assert_eq!(cell_char(1.0, false, r), '@');
        assert_eq!(cell_char(0.0, true, r), '@');
        assert_eq!(cell_char(1.0, true, r), ' ');
    }

    #[test]
    fn grid_never_divides_by_zero() {
        assert_eq!(grid_size((0, 0), 120).2, 1);
        let (cw, ch, rows) = grid_size((1600, 1000), 120);
        assert_eq!(cw, 13);
        assert_eq!(ch, 26);
        assert_eq!(rows, 38);
    }

    #[test]
    fn render_is_black_behind_a_space_cell() {
        let src = RgbaImage::from_pixel(1, 1, Rgba([0, 0, 0, 255]));
        let params = AsciiParams::default();
        let out = render(&src, &params, (120, 60), [255, 255, 255]);
        assert_eq!((out.width(), out.height()), (120, 60));
        assert!(out.pixels().all(|p| p.0[..3] == [0, 0, 0]));
    }

    #[test]
    fn render_paints_light_cells_with_the_accent() {
        let src = RgbaImage::from_pixel(1, 1, Rgba([255, 255, 255, 255]));
        let params = AsciiParams {
            columns: 4,
            color: AsciiColor::Accent,
            ..Default::default()
        };
        let out = render(&src, &params, (64, 64), [200, 10, 10]);
        assert!(
            out.pixels().any(|p| p.0[0] > 100 && p.0[1] < 60),
            "accent tint missing"
        );
    }

    #[test]
    fn ascii_support_is_png_jpeg_only() {
        assert!(supports_ascii(Path::new("a.PNG")));
        assert!(supports_ascii(Path::new("a.jpeg")));
        assert!(supports_ascii(Path::new("a.jpg")));
        assert!(!supports_ascii(Path::new("a.webp")));
    }

    #[test]
    fn cache_key_buckets_the_target_size() {
        let params = AsciiParams::default();
        let a = cache_key(Path::new("/w/a.png"), &params, (1600, 1000));
        assert_eq!(
            a,
            cache_key(Path::new("/w/a.png"), &params, (1700, 1010)),
            "same 256 px bucket"
        );
        assert_ne!(a, cache_key(Path::new("/w/a.png"), &params, (2000, 1000)));
        assert_ne!(
            a,
            cache_key(
                Path::new("/w/a.png"),
                &AsciiParams {
                    invert: true,
                    ..params
                },
                (1600, 1000)
            )
        );
    }

    #[test]
    fn a_corrupt_source_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.png");
        std::fs::write(&path, b"not a png").unwrap();
        assert!(load_source(&path).is_err());
    }

    #[test]
    fn a_render_is_skipped_while_one_holds_the_right_key() {
        assert!(should_render(None, false, "k"));
        assert!(!should_render(Some("k"), false, "k"));
        assert!(should_render(Some("old"), false, "k"));
        assert!(!should_render(None, true, "k"));
    }
}
