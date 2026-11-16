//! Keeps the images of a message within the size the API accepts.

use std::io::Cursor;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use image::imageops::FilterType;
use image::{DynamicImage, ImageDecoder, ImageReader};

/// The longest side an image may have. The API allows less per image once a
/// request carries many of them, so a larger image that was accepted early in
/// a conversation would later make every request fail.
const MAX_SIDE: u32 = 2000;

/// Enough base64 to hold the header that states an image's size.
const HEADER_BASE64: usize = 64 * 1024;

const DATA: &[u8] = br#""data":""#;

/// How the base64 of a PNG, JPEG, GIF and WebP file starts.
const IMAGE_STARTS: [&[u8]; 4] = [b"iVBORw0KGgo", b"/9j/", b"R0lGOD", b"UklGR"];

/// The message's JSON with every image larger than [`MAX_SIDE`] scaled down
/// to it, in its own format; `None` when no image had to change. Only the
/// base64 data is replaced, so the layout of the JSON stays as it was.
pub(crate) fn fit(json: &[u8]) -> Option<Vec<u8>> {
    let mut out: Option<Vec<u8>> = None;
    let (mut copied, mut at) = (0, 0);
    // Quotes inside a text are escaped, so this only matches a real `data` key.
    while let Some(found) = json[at..].windows(DATA.len()).position(|w| w == DATA) {
        let start = at + found + DATA.len();
        let len = json[start..].iter().position(|&b| b == b'"').unwrap_or(json.len() - start);
        let data = &json[start..start + len];
        at = start + len;
        if !IMAGE_STARTS.iter().any(|s| data.starts_with(s)) {
            continue;
        }
        let Some(smaller) = scaled_down(data) else { continue };
        let out = out.get_or_insert_with(|| Vec::with_capacity(json.len()));
        out.extend_from_slice(&json[copied..start]);
        out.extend_from_slice(smaller.as_bytes());
        copied = at;
    }
    let mut out = out?;
    out.extend_from_slice(&json[copied..]);
    Some(out)
}

fn fits(width: u32, height: u32) -> bool {
    width.max(height) <= MAX_SIDE
}

/// The image as base64 again, scaled to fit; `None` when it already fits or
/// cannot be read (it is then left for the API to judge).
fn scaled_down(data: &[u8]) -> Option<String> {
    // Most images fit: learn that from the header alone, without decoding them whole.
    let header = STANDARD.decode(&data[..data.len().min(HEADER_BASE64)]).ok();
    let size = header
        .and_then(|bytes| ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?.into_dimensions().ok());
    if size.is_some_and(|(width, height)| fits(width, height)) {
        return None;
    }
    let bytes = STANDARD.decode(data).ok()?;
    let reader = ImageReader::new(Cursor::new(&bytes)).with_guessed_format().ok()?;
    let format = reader.format()?;
    let mut decoder = reader.into_decoder().ok()?;
    let orientation = decoder.orientation().ok()?;
    let mut image = DynamicImage::from_decoder(decoder).ok()?;
    if fits(image.width(), image.height()) {
        return None;
    }
    // Re-encoding drops the photo's rotation note, so turn the pixels instead.
    image.apply_orientation(orientation);
    let mut out = Cursor::new(Vec::new());
    image.resize(MAX_SIDE, MAX_SIDE, FilterType::CatmullRom).write_to(&mut out, format).ok()?;
    Some(STANDARD.encode(out.into_inner()))
}

#[cfg(test)]
mod tests {
    use image::ImageFormat;

    use super::*;

    fn encoded(width: u32, height: u32, format: ImageFormat) -> String {
        let mut out = Cursor::new(Vec::new());
        DynamicImage::new_rgb8(width, height).write_to(&mut out, format).expect("encodes");
        STANDARD.encode(out.into_inner())
    }

    fn message(media_type: &str, data: &str) -> String {
        format!(
            r#"{{"role":"user","content":[{{"type":"text","text":"look"}},{{"type":"image","source":{{"type":"base64","media_type":"{media_type}","data":"{data}"}}}}]}}"#
        )
    }

    fn size_in(json: &[u8]) -> (u32, u32) {
        let value: serde_json::Value = serde_json::from_slice(json).expect("valid json");
        let data = value["content"][1]["source"]["data"].as_str().expect("image data");
        let bytes = STANDARD.decode(data).expect("base64");
        ImageReader::new(Cursor::new(bytes)).with_guessed_format().expect("format").into_dimensions().expect("size")
    }

    #[test]
    fn an_image_that_is_too_large_is_scaled_down_in_its_own_format() {
        for (format, media_type) in [(ImageFormat::Png, "image/png"), (ImageFormat::Jpeg, "image/jpeg")] {
            let json = message(media_type, &encoded(4000, 1000, format));
            let fitted = fit(json.as_bytes()).expect("changed");
            assert_eq!(size_in(&fitted), (2000, 500));
            assert!(fitted.ends_with(b"}]}"), "the layout is kept");
            assert!(fit(&fitted).is_none(), "nothing left to change");
        }
    }

    #[test]
    fn an_image_that_fits_and_other_data_are_left_alone() {
        let json = message("image/png", &encoded(2000, 300, ImageFormat::Png));
        assert!(fit(json.as_bytes()).is_none());
        assert!(fit(message("application/pdf", "JVBERi0xLjQK").as_bytes()).is_none());
        assert!(fit(message("image/png", "iVBORw0KGgoAAAA=").as_bytes()).is_none(), "unreadable: left as it is");
    }
}
