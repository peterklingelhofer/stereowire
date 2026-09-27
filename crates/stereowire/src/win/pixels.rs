//! The pixel work behind the Windows sender, kept apart from any Windows API
//! so it can be tested directly: drawing the pointer into a captured desktop
//! image, halving an image, and converting BGRA to NV12.
//!
//! Every image here is tightly packed BGRA, four bytes a pixel, rows back to
//! back.

/// A pointer shape as Desktop Duplication reports it.
pub struct CursorShape {
    pub kind: CursorKind,
    pub width: usize,
    /// Rows in `data`. A monochrome shape stacks two masks, so it draws half
    /// as many.
    pub height: usize,
    /// Bytes from one row of `data` to the next.
    pub pitch: usize,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CursorKind {
    /// A 1 bpp AND mask, then a 1 bpp XOR mask of the same size.
    Monochrome,
    /// 32 bpp BGRA with straight alpha.
    Color,
    /// 32 bpp BGRA whose alpha byte says whether to replace the screen pixel
    /// (0) or XOR it (0xFF).
    MaskedColor,
}

/// Draws `shape` into `image` with its top-left corner at (`x`, `y`),
/// clipped to the image.
pub fn draw_cursor(
    image: &mut [u8],
    width: usize,
    height: usize,
    shape: &CursorShape,
    x: i32,
    y: i32,
) {
    let rows = match shape.kind {
        CursorKind::Monochrome => shape.height / 2,
        CursorKind::Color | CursorKind::MaskedColor => shape.height,
    };
    for row in 0..rows {
        let Some(target_y) = offset(y, row, height) else {
            continue;
        };
        for column in 0..shape.width {
            let Some(target_x) = offset(x, column, width) else {
                continue;
            };
            let at = (target_y * width + target_x) * 4;
            let Some(pixel) = image.get_mut(at..at + 3) else {
                continue;
            };
            match shape.kind {
                CursorKind::Monochrome => {
                    let byte = column / 8;
                    let bit = 0x80u8 >> (column % 8);
                    let mask = |mask_row: usize| {
                        shape
                            .data
                            .get(mask_row * shape.pitch + byte)
                            .map(|&b| b & bit != 0)
                    };
                    // A shape too short for its own size leaves the pixel be
                    let (Some(and), Some(xor)) = (mask(row), mask(row + rows)) else {
                        continue;
                    };
                    for channel in pixel.iter_mut() {
                        let kept = if and { *channel } else { 0 };
                        *channel = if xor { !kept } else { kept };
                    }
                }
                CursorKind::Color | CursorKind::MaskedColor => {
                    let source = row * shape.pitch + column * 4;
                    let Some(source) = shape.data.get(source..source + 4) else {
                        continue;
                    };
                    let alpha = u32::from(source[3]);
                    for (channel, &value) in pixel.iter_mut().zip(source) {
                        *channel = match shape.kind {
                            CursorKind::MaskedColor if alpha == 0 => value,
                            CursorKind::MaskedColor => *channel ^ value,
                            _ => {
                                ((u32::from(value) * alpha
                                    + u32::from(*channel) * (255 - alpha)
                                    + 127)
                                    / 255) as u8
                            }
                        };
                    }
                }
            }
        }
    }
}

/// `origin + step` as an index below `limit`, or `None` when it falls outside.
fn offset(origin: i32, step: usize, limit: usize) -> Option<usize> {
    let position = i64::from(origin) + step as i64;
    (0..limit as i64)
        .contains(&position)
        .then_some(position as usize)
}

/// Halves `source` in both directions by averaging each 2x2 block, writing
/// into `out`, and returns the new size. The result is trimmed to even
/// dimensions, as NV12 needs.
pub fn downscale_half(
    source: &[u8],
    width: usize,
    height: usize,
    out: &mut Vec<u8>,
) -> (usize, usize) {
    let out_width = (width / 2) & !1;
    let out_height = (height / 2) & !1;
    out.clear();
    out.resize(out_width * out_height * 4, 0);
    let stride = width * 4;
    for (y, out_row) in out.chunks_exact_mut(out_width * 4).enumerate() {
        let top = &source[2 * y * stride..];
        let bottom = &source[(2 * y + 1) * stride..];
        for (x, pixel) in out_row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let at = x * 8;
            for (channel, value) in pixel.iter_mut().enumerate() {
                let sum = u32::from(top[at + channel])
                    + u32::from(top[at + 4 + channel])
                    + u32::from(bottom[at + channel])
                    + u32::from(bottom[at + 4 + channel]);
                *value = ((sum + 2) / 4) as u8;
            }
        }
    }
    (out_width, out_height)
}

/// BT.709 coefficients for full-range RGB to limited-range YCbCr, scaled by
/// 2^16. Each chroma row sums to zero, so grey stays exactly neutral.
const Y_COEFFICIENTS: [i32; 3] = [11_966, 40_254, 4_064];
const CB_COEFFICIENTS: [i32; 3] = [-6_596, -22_189, 28_785];
const CR_COEFFICIENTS: [i32; 3] = [28_784, -26_145, -2_639];

/// Converts `source`, whose dimensions must be even, to limited-range BT.709
/// NV12 in `out`, reusing its allocation. Chroma comes from the average of
/// each 2x2 block.
pub fn bgra_to_nv12(source: &[u8], width: usize, height: usize, out: &mut Vec<u8>) {
    out.clear();
    out.resize(width * height * 3 / 2, 0);
    let (luma, chroma) = out.split_at_mut(width * height);
    let stride = width * 4;
    for (pair, uv_row) in chroma.chunks_exact_mut(width).enumerate() {
        let y = pair * 2;
        let rows = [
            &source[y * stride..(y + 1) * stride],
            &source[(y + 1) * stride..(y + 2) * stride],
        ];
        for (row, source_row) in rows.iter().enumerate() {
            let luma_row = &mut luma[(y + row) * width..(y + row + 1) * width];
            for (value, pixel) in luma_row.iter_mut().zip(source_row.as_chunks::<4>().0) {
                *value = luma_of(pixel);
            }
        }
        for (x, uv) in uv_row.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            let at = x * 8;
            // Summed B, G and R of the block's four pixels
            let mut sum = [0i32; 3];
            for source_row in rows {
                for pixel in source_row[at..at + 8].as_chunks::<4>().0 {
                    for (total, &value) in sum.iter_mut().zip(pixel) {
                        *total += i32::from(value);
                    }
                }
            }
            let [b, g, r] = sum;
            uv[0] = chroma_of(CB_COEFFICIENTS, r, g, b);
            uv[1] = chroma_of(CR_COEFFICIENTS, r, g, b);
        }
    }
}

fn luma_of(pixel: &[u8]) -> u8 {
    let [r, g, b] = Y_COEFFICIENTS;
    let value = r * i32::from(pixel[2])
        + g * i32::from(pixel[1])
        + b * i32::from(pixel[0])
        + (16 << 16)
        + (1 << 15);
    (value >> 16).clamp(0, 255) as u8
}

/// One chroma sample from the sums of four pixels, hence the two extra bits
/// of shift.
fn chroma_of(coefficients: [i32; 3], r: i32, g: i32, b: i32) -> u8 {
    let [cr, cg, cb] = coefficients;
    let value = cr * r + cg * g + cb * b + (128 << 18) + (1 << 17);
    (value >> 18).clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: usize, height: usize, bgra: [u8; 4]) -> Vec<u8> {
        bgra.repeat(width * height)
    }

    #[test]
    fn solid_colours_convert_to_their_bt709_limited_range_values() {
        // (B, G, R) and the expected (Y, Cb, Cr)
        let cases = [
            ([0, 0, 0], [16, 128, 128]),
            ([255, 255, 255], [235, 128, 128]),
            ([128, 128, 128], [126, 128, 128]),
            ([0, 0, 255], [63, 102, 240]),
            ([0, 255, 0], [173, 42, 26]),
            ([255, 0, 0], [32, 240, 118]),
        ];
        for ([b, g, r], expected) in cases {
            let image = solid(4, 2, [b, g, r, 255]);
            let mut nv12 = Vec::new();
            bgra_to_nv12(&image, 4, 2, &mut nv12);
            assert_eq!(nv12.len(), 12);
            let got = [nv12[0], nv12[8], nv12[9]];
            for (got, want) in got.iter().zip(expected) {
                assert!(
                    got.abs_diff(want) <= 1,
                    "BGR {b},{g},{r} gave YCbCr {got:?}, wanted {expected:?}"
                );
            }
            assert!(nv12[..8].iter().all(|&y| y == nv12[0]));
            assert_eq!(&nv12[8..10], &nv12[10..12]);
        }
    }

    #[test]
    fn chroma_comes_from_the_average_of_each_block() {
        // The left block is red over blue, which averages to purple, and the
        // right block is black. Luma follows each pixel
        let mut image = solid(4, 2, [0, 0, 0, 255]);
        image[..8].copy_from_slice(&[0, 0, 255, 255, 0, 0, 255, 255]);
        image[16..24].copy_from_slice(&[255, 0, 0, 255, 255, 0, 0, 255]);
        let mut nv12 = Vec::new();
        bgra_to_nv12(&image, 4, 2, &mut nv12);
        assert_eq!(&nv12[..8], &[63, 63, 16, 16, 32, 32, 16, 16]);
        assert_eq!(&nv12[8..], &[171, 179, 128, 128]);
    }

    #[test]
    fn halving_averages_each_block() {
        // A one-pixel checkerboard averages to mid grey everywhere
        let mut board = Vec::new();
        for y in 0..4 {
            for x in 0..4 {
                let value = if (x + y) % 2 == 0 { 255 } else { 0 };
                board.extend_from_slice(&[value, value, value, 255]);
            }
        }
        let mut half = Vec::new();
        assert_eq!(downscale_half(&board, 4, 4, &mut half), (2, 2));
        assert!(half
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| *p == [128, 128, 128, 255]));

        // A checkerboard of 2x2 squares keeps its squares, one pixel each
        let mut squares = Vec::new();
        for y in 0..4 {
            for x in 0..4 {
                let value = if (x / 2 + y / 2) % 2 == 0 { 200 } else { 10 };
                squares.extend_from_slice(&[value, value, value, 255]);
            }
        }
        downscale_half(&squares, 4, 4, &mut half);
        let firsts: Vec<u8> = half.as_chunks::<4>().0.iter().map(|p| p[0]).collect();
        assert_eq!(firsts, vec![200, 10, 10, 200]);
    }

    #[test]
    fn halving_trims_to_even_dimensions() {
        let image = solid(6, 6, [1, 2, 3, 4]);
        let mut half = Vec::new();
        // 6x6 halves to 3x3, then trims to 2x2
        assert_eq!(downscale_half(&image, 6, 6, &mut half), (2, 2));
        assert_eq!(half.len(), 2 * 2 * 4);
    }

    /// A 2x2 monochrome shape. AND mask rows first, then XOR mask rows, one
    /// byte a row, most significant bit first.
    fn monochrome(and: [u8; 2], xor: [u8; 2]) -> CursorShape {
        CursorShape {
            kind: CursorKind::Monochrome,
            width: 2,
            height: 4,
            pitch: 1,
            data: vec![and[0], and[1], xor[0], xor[1]],
        }
    }

    #[test]
    fn a_monochrome_cursor_inverts_where_its_xor_mask_says() {
        let mut image = solid(3, 3, [10, 20, 30, 255]);
        // Top-left pixel: AND 1, XOR 1 inverts. Top-right: AND 0, XOR 0 is
        // black. Bottom-left: AND 1, XOR 0 leaves the screen. Bottom-right:
        // AND 0, XOR 1 is white
        let shape = monochrome([0b1000_0000, 0b1000_0000], [0b1000_0000, 0b0100_0000]);
        draw_cursor(&mut image, 3, 3, &shape, 0, 0);
        let pixel = |x: usize, y: usize| &image[(y * 3 + x) * 4..(y * 3 + x) * 4 + 3];
        assert_eq!(pixel(0, 0), &[245, 235, 225]);
        assert_eq!(pixel(1, 0), &[0, 0, 0]);
        assert_eq!(pixel(0, 1), &[10, 20, 30]);
        assert_eq!(pixel(1, 1), &[255, 255, 255]);
        // Outside the shape nothing changes
        assert_eq!(pixel(2, 2), &[10, 20, 30]);
    }

    #[test]
    fn a_cursor_is_clipped_at_the_image_edges() {
        let mut image = solid(2, 2, [10, 20, 30, 255]);
        let shape = monochrome([0, 0], [0, 0]);
        // Only the shape's bottom-right pixel lands on the image
        draw_cursor(&mut image, 2, 2, &shape, -1, -1);
        assert_eq!(&image[..3], &[0, 0, 0]);
        assert_eq!(&image[4..7], &[10, 20, 30]);
        // Entirely off the image draws nothing and does not panic
        draw_cursor(&mut image, 2, 2, &shape, 5, 5);
        draw_cursor(&mut image, 2, 2, &shape, i32::MIN, i32::MAX);
    }

    #[test]
    fn colour_cursors_blend_by_alpha_and_masked_ones_replace_or_xor() {
        let mut image = solid(2, 1, [100, 100, 100, 255]);
        let colour = CursorShape {
            kind: CursorKind::Color,
            width: 2,
            height: 1,
            pitch: 8,
            // Opaque white, then half-transparent black
            data: vec![255, 255, 255, 255, 0, 0, 0, 128],
        };
        draw_cursor(&mut image, 2, 1, &colour, 0, 0);
        assert_eq!(&image[..3], &[255, 255, 255]);
        assert_eq!(&image[4..7], &[50, 50, 50]);

        let mut image = solid(2, 1, [0x0f, 0x0f, 0x0f, 255]);
        let masked = CursorShape {
            kind: CursorKind::MaskedColor,
            width: 2,
            height: 1,
            pitch: 8,
            data: vec![1, 2, 3, 0, 0xff, 0xff, 0xff, 0xff],
        };
        draw_cursor(&mut image, 2, 1, &masked, 0, 0);
        assert_eq!(&image[..3], &[1, 2, 3]);
        assert_eq!(&image[4..7], &[0xf0, 0xf0, 0xf0]);
    }
}
