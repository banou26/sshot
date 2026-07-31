use std::process::{Command, Stdio};

use anyhow::{Context, Result};

/// Capture all outputs via grim. Returns raw BGRA data + width, height, stride.
pub fn capture_workspace() -> Result<(Vec<u8>, u32, u32, u32)> {
    let output = Command::new("grim")
        .args(["-t", "ppm", "-"])
        .stdout(Stdio::piped())
        .output()
        .context("Failed to run grim (is it installed?)")?;

    if !output.status.success() {
        anyhow::bail!("grim failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    parse_ppm(&output.stdout)
}

/// Parse a PPM P6 image into BGRA pixel data.
fn parse_ppm(data: &[u8]) -> Result<(Vec<u8>, u32, u32, u32)> {
    let mut pos = 0;

    if !data.starts_with(b"P6") {
        anyhow::bail!("Not a PPM P6 file");
    }
    pos = find_byte(b'\n', data, pos)? + 1;

    // Skip comment lines
    while pos < data.len() && data[pos] == b'#' {
        pos = find_byte(b'\n', data, pos)? + 1;
    }

    // Parse "width height\n"
    let line_end = find_byte(b'\n', data, pos)?;
    let dims = std::str::from_utf8(&data[pos..line_end]).context("Invalid PPM dimensions")?;
    let mut parts = dims.split_whitespace();
    let width: u32 = parts.next().context("No width in PPM")?.parse()?;
    let height: u32 = parts.next().context("No height in PPM")?.parse()?;
    pos = line_end + 1;

    // Skip "255\n" (max value)
    pos = find_byte(b'\n', data, pos)? + 1;

    if width == 0 || height == 0 {
        anyhow::bail!("PPM has zero size ({width}x{height})");
    }
    let pixel_count = (width as usize)
        .checked_mul(height as usize)
        .context("PPM dimensions overflow")?;

    let rgb = &data[pos..];
    if rgb.len() < pixel_count * 3 {
        anyhow::bail!(
            "PPM body is short: expected {} bytes for {width}x{height}, got {}",
            pixel_count * 3,
            rgb.len()
        );
    }

    // Convert RGB to BGRA (Wayland Argb8888 is BGRA on little endian).
    // Paired chunk iteration lets the bounds check happen once for the whole
    // buffer instead of once per pixel.
    let stride = width * 4;
    let mut bgra = vec![0u8; pixel_count * 4];
    for (d, s) in bgra.chunks_exact_mut(4).zip(rgb.chunks_exact(3)) {
        d[0] = s[2];
        d[1] = s[1];
        d[2] = s[0];
        d[3] = 255;
    }

    Ok((bgra, width, height, stride))
}

fn find_byte(byte: u8, data: &[u8], start: usize) -> Result<usize> {
    data[start..].iter().position(|&b| b == byte)
        .map(|p| start + p)
        .context("Unexpected end of PPM data")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ppm(width: u32, height: u32, body: &[u8]) -> Vec<u8> {
        let mut v = format!("P6\n{width} {height}\n255\n").into_bytes();
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn parses_a_minimal_image_and_swizzles_to_bgra() {
        // One red pixel, one green pixel.
        let data = ppm(2, 1, &[255, 0, 0, 0, 255, 0]);
        let (bgra, w, h, stride) = parse_ppm(&data).unwrap();
        assert_eq!((w, h, stride), (2, 1, 8));
        assert_eq!(bgra, vec![0, 0, 255, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn skips_comment_lines() {
        let mut data = b"P6\n# made by grim\n".to_vec();
        data.extend_from_slice(b"1 1\n255\n");
        data.extend_from_slice(&[1, 2, 3]);
        let (bgra, ..) = parse_ppm(&data).unwrap();
        assert_eq!(bgra, vec![3, 2, 1, 255]);
    }

    #[test]
    fn rejects_a_truncated_body_instead_of_returning_transparent_pixels() {
        // Claims 4 pixels, supplies 1. The old loop silently left the rest zeroed.
        let data = ppm(2, 2, &[1, 2, 3]);
        let err = parse_ppm(&data).unwrap_err().to_string();
        assert!(err.contains("short"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_zero_and_non_ppm_input() {
        assert!(parse_ppm(&ppm(0, 5, &[])).is_err());
        assert!(parse_ppm(b"P3\n1 1\n255\n").is_err());
        assert!(parse_ppm(b"").is_err());
    }
}
