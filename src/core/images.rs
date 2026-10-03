use std::fmt;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

pub const MAX_INPUT_IMAGES: usize = 4;
pub const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_IMAGE_TOTAL_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_IMAGE_DIMENSION: u32 = 4096;
pub const MAX_IMAGE_TOTAL_PIXELS: u64 = 16_777_216;
pub const MAX_IMAGE_BASE64_BYTES: usize = MAX_IMAGE_BYTES.div_ceil(3) * 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageValidationError {
    Invalid,
    TooLarge,
}

#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ValidatedImage {
    data_url: String,
    #[serde(skip)]
    byte_len: usize,
    #[serde(skip)]
    width: u32,
    #[serde(skip)]
    height: u32,
}

impl fmt::Debug for ValidatedImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ValidatedImage")
            .field("byte_len", &self.byte_len)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}

impl ValidatedImage {
    /// Checks the envelope and dimensions without decompressing image pixels.
    pub fn from_data_url(data_url: String) -> Result<Self, ImageValidationError> {
        let (png, data) = if let Some(data) = data_url.strip_prefix("data:image/png;base64,") {
            (true, data)
        } else if let Some(data) = data_url.strip_prefix("data:image/jpeg;base64,") {
            (false, data)
        } else {
            return Err(ImageValidationError::Invalid);
        };
        if data.len() > MAX_IMAGE_BASE64_BYTES {
            return Err(ImageValidationError::TooLarge);
        }
        let bytes = STANDARD
            .decode(data)
            .map_err(|_| ImageValidationError::Invalid)?;
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(ImageValidationError::TooLarge);
        }
        let (width, height) = if png {
            png_dimensions(&bytes)
        } else {
            jpeg_dimensions(&bytes)
        }
        .ok_or(ImageValidationError::Invalid)?;
        if width == 0 || height == 0 {
            return Err(ImageValidationError::Invalid);
        }
        if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
            return Err(ImageValidationError::TooLarge);
        }
        Ok(Self {
            data_url,
            byte_len: bytes.len(),
            width,
            height,
        })
    }

    pub fn data_url(&self) -> &str {
        &self.data_url
    }
    pub fn byte_len(&self) -> usize {
        self.byte_len
    }
    pub fn pixels(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

impl<'de> Deserialize<'de> for ValidatedImage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            data_url: String,
        }
        let raw = Raw::deserialize(deserializer)?;
        Self::from_data_url(raw.data_url).map_err(|_| D::Error::custom("invalid inline image"))
    }
}

#[derive(Default)]
pub struct ImageBudget {
    count: usize,
    bytes: usize,
    pixels: u64,
}

impl ImageBudget {
    pub fn add(&mut self, image: &ValidatedImage) -> Result<(), ImageValidationError> {
        self.count = self
            .count
            .checked_add(1)
            .ok_or(ImageValidationError::TooLarge)?;
        self.bytes = self
            .bytes
            .checked_add(image.byte_len())
            .ok_or(ImageValidationError::TooLarge)?;
        self.pixels = self
            .pixels
            .checked_add(image.pixels())
            .ok_or(ImageValidationError::TooLarge)?;
        if self.count > MAX_INPUT_IMAGES
            || self.bytes > MAX_IMAGE_TOTAL_BYTES
            || self.pixels > MAX_IMAGE_TOTAL_PIXELS
        {
            return Err(ImageValidationError::TooLarge);
        }
        Ok(())
    }
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    let mut at = 8usize;
    let mut dimensions = None;
    let mut data_seen = false;
    while at < bytes.len() {
        let length =
            u32::from_be_bytes(bytes.get(at..at.checked_add(4)?)?.try_into().ok()?) as usize;
        let kind = bytes.get(at + 4..at + 8)?;
        let end = at.checked_add(12)?.checked_add(length)?;
        let data = bytes.get(at + 8..end.checked_sub(4)?)?;
        bytes.get(..end)?;
        match kind {
            b"IHDR" if at == 8 && length == 13 => {
                let bit_depth = data[8];
                let valid_depth = match data[9] {
                    0 => matches!(bit_depth, 1 | 2 | 4 | 8 | 16),
                    2 | 4 | 6 => matches!(bit_depth, 8 | 16),
                    3 => matches!(bit_depth, 1 | 2 | 4 | 8),
                    _ => false,
                };
                if !valid_depth || data[10] != 0 || data[11] != 0 || data[12] > 1 {
                    return None;
                }
                dimensions = Some((
                    u32::from_be_bytes(data[..4].try_into().ok()?),
                    u32::from_be_bytes(data[4..8].try_into().ok()?),
                ));
            }
            b"IHDR" | b"acTL" | b"fcTL" | b"fdAT" => return None,
            b"IDAT" if dimensions.is_some() => data_seen |= !data.is_empty(),
            b"IEND" if length == 0 && data_seen && end == bytes.len() => return dimensions,
            b"IEND" => return None,
            _ if dimensions.is_none() => return None,
            _ => {}
        }
        at = end;
    }
    None
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    let mut at = 2usize;
    let mut dimensions = None;
    let mut scan_seen = false;
    while at < bytes.len() {
        if bytes.get(at) != Some(&0xff) {
            return None;
        }
        while bytes.get(at) == Some(&0xff) {
            at += 1;
        }
        let marker = *bytes.get(at)?;
        at += 1;
        if marker == 0xd9 {
            return (scan_seen && at == bytes.len())
                .then_some(dimensions)
                .flatten();
        }
        if matches!(marker, 0 | 0xd8 | 0xd0..=0xd7) {
            return None;
        }
        let length = usize::from(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?));
        if length < 2 {
            return None;
        }
        let end = at.checked_add(length)?;
        let data = bytes.get(at + 2..end)?;
        match marker {
            0xc0 | 0xc2 => {
                if dimensions.is_some()
                    || data.len() < 6
                    || data[0] != 8
                    || !matches!(data[5], 1 | 3 | 4)
                    || data.len() != 6 + usize::from(data[5]) * 3
                {
                    return None;
                }
                dimensions = Some((
                    u32::from(u16::from_be_bytes(data[3..5].try_into().ok()?)),
                    u32::from(u16::from_be_bytes(data[1..3].try_into().ok()?)),
                ));
            }
            0xc1 | 0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf | 0xdc => return None,
            0xda => {
                if dimensions.is_none()
                    || data.len() < 4
                    || data[0] == 0
                    || data.len() != 4 + usize::from(data[0]) * 2
                {
                    return None;
                }
                scan_seen = true;
            }
            _ => {}
        }
        at = end;
        if marker == 0xda {
            loop {
                let byte = *bytes.get(at)?;
                at += 1;
                if byte != 0xff {
                    continue;
                }
                let marker_at = at - 1;
                while bytes.get(at) == Some(&0xff) {
                    at += 1;
                }
                match *bytes.get(at)? {
                    0 | 0xd0..=0xd7 => at += 1,
                    _ => {
                        at = marker_at;
                        break;
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl2n3QAAAAASUVORK5CYII=";

    fn url(bytes: &[u8], mime: &str) -> String {
        format!("data:{mime};base64,{}", STANDARD.encode(bytes))
    }

    #[test]
    fn inline_image_checks_envelope_bounds_and_redacts_debug() {
        let image = ValidatedImage::from_data_url(format!("data:image/png;base64,{PNG}")).unwrap();
        assert_eq!(image.pixels(), 1);
        assert!(!format!("{image:?}").contains(PNG));
        let bytes = STANDARD.decode(PNG).unwrap();
        for bad in [
            "https://example.test/image.png".to_owned(),
            "file:///private/image.png".to_owned(),
            format!("data:image/png;base64,{PNG}\n"),
            url(&bytes, "image/jpeg"),
            url(&bytes[..bytes.len() - 1], "image/png"),
            url(&[bytes.clone(), vec![0]].concat(), "image/png"),
        ] {
            assert_eq!(
                ValidatedImage::from_data_url(bad),
                Err(ImageValidationError::Invalid)
            );
        }
        let mut huge = bytes.clone();
        huge[16..20].copy_from_slice(&4097u32.to_be_bytes());
        assert_eq!(
            ValidatedImage::from_data_url(url(&huge, "image/png")),
            Err(ImageValidationError::TooLarge)
        );
        let mut animated = bytes;
        animated[37..41].copy_from_slice(b"acTL");
        assert_eq!(
            ValidatedImage::from_data_url(url(&animated, "image/png")),
            Err(ImageValidationError::Invalid)
        );
        assert_eq!(
            ValidatedImage::from_data_url(format!(
                "data:image/png;base64,{}",
                "A".repeat(MAX_IMAGE_BASE64_BYTES + 4)
            )),
            Err(ImageValidationError::TooLarge)
        );
    }

    #[test]
    fn jpeg_envelopes_require_one_supported_frame_and_complete_scan() {
        // Minimal structural fixture; the gateway never decodes entropy-coded pixels.
        let bytes = [
            0xff, 0xd8, 0xff, 0xc0, 0, 11, 8, 0, 1, 0, 2, 1, 1, 0x11, 0, 0xff, 0xda, 0, 8, 1, 1, 0,
            0, 63, 0, 0x12, 0xff, 0, 0x34, 0xff, 0xd9,
        ];
        let image = ValidatedImage::from_data_url(url(&bytes, "image/jpeg")).unwrap();
        assert_eq!(image.pixels(), 2);
        for end in 0..bytes.len() {
            assert!(ValidatedImage::from_data_url(url(&bytes[..end], "image/jpeg")).is_err());
        }
        let mut oversized = bytes;
        oversized[9..11].copy_from_slice(&4097u16.to_be_bytes());
        assert_eq!(
            ValidatedImage::from_data_url(url(&oversized, "image/jpeg")),
            Err(ImageValidationError::TooLarge)
        );
        let mut unsupported = bytes;
        unsupported[3] = 0xc3;
        assert!(ValidatedImage::from_data_url(url(&unsupported, "image/jpeg")).is_err());
    }

    #[test]
    fn request_budget_caps_count_pixels_and_combined_file_bytes() {
        let mut image =
            ValidatedImage::from_data_url(format!("data:image/png;base64,{PNG}")).unwrap();
        let mut budget = ImageBudget::default();
        for _ in 0..MAX_INPUT_IMAGES {
            budget.add(&image).unwrap();
        }
        assert_eq!(budget.add(&image), Err(ImageValidationError::TooLarge));
        image.width = 4096;
        image.height = 4096;
        let mut budget = ImageBudget::default();
        budget.add(&image).unwrap();
        assert_eq!(budget.add(&image), Err(ImageValidationError::TooLarge));
        image.width = 1;
        image.height = 1;
        image.byte_len = MAX_IMAGE_BYTES;
        let mut budget = ImageBudget::default();
        budget.add(&image).unwrap();
        budget.add(&image).unwrap();
        assert_eq!(budget.add(&image), Err(ImageValidationError::TooLarge));
    }
}
