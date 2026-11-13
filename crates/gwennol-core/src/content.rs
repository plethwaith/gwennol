//! What a user turn, and a tool's result, can carry: text, images, and
//! references to images the provider resolves.
//!
//! [`Session::turn_with`](crate::Session::turn_with) takes a list of
//! [`UserContent`] and stores it as one run of blocks, in the order given.
//! An [`Image`] is built from a file's bytes alone: its format is read
//! from how the bytes start, so a caller never labels one. An
//! [`ImageReference`] is the exception: there are no bytes to read a
//! format from, so the caller declares it.
//!
//! A tool's result is a [`ToolContent`]: text alone, or a list of the same
//! items.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use gwead::serde_json::{Value, json};

/// One item of a user turn or of a tool's result, in the position it was
/// given.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum UserContent {
    /// Text.
    Text(String),
    /// An image.
    Image(Image),
    /// An image named by a reference the provider resolves.
    ImageReference(ImageReference),
}

impl UserContent {
    /// The `LLM_CHAT` content block for this item: a `text` block, or an
    /// `image` block with a `base64` source (standard, padded) or a
    /// `reference` source.
    pub(crate) fn block(&self) -> Value {
        match self {
            Self::Text(text) => json!({"type": "text", "text": text}),
            Self::Image(image) => json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": image.format.media_type(),
                    "data": STANDARD.encode(&image.bytes),
                },
            }),
            Self::ImageReference(image) => json!({
                "type": "image",
                "source": {
                    "type": "reference",
                    "media_type": image.format.media_type(),
                    "reference": image.reference,
                },
            }),
        }
    }
}

/// What a tool's result carries, as the model gets it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToolContent {
    /// Text alone: the contract's string form.
    Text(String),
    /// Text, images and image references, in the order the tool gave
    /// them: the contract's list form. Never empty.
    Blocks(Vec<UserContent>),
}

impl ToolContent {
    /// The `content` of a `tool_result` block: the string, or the list of
    /// each item's block.
    pub(crate) fn value(&self) -> Value {
        match self {
            Self::Text(text) => Value::String(text.clone()),
            Self::Blocks(items) => Value::Array(items.iter().map(UserContent::block).collect()),
        }
    }
}

impl From<&str> for UserContent {
    fn from(text: &str) -> Self {
        Self::Text(text.to_string())
    }
}

impl From<String> for UserContent {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl From<Image> for UserContent {
    fn from(image: Image) -> Self {
        Self::Image(image)
    }
}

impl From<ImageReference> for UserContent {
    fn from(image: ImageReference) -> Self {
        Self::ImageReference(image)
    }
}

/// The image formats the `LLM_CHAT` contract names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ImageFormat {
    /// JPEG.
    Jpeg,
    /// PNG.
    Png,
    /// GIF, either version.
    Gif,
    /// WebP.
    Webp,
}

impl ImageFormat {
    /// Every format, in the order the contract's `ImageMediaType` enum lists
    /// them.
    pub const ALL: &'static [ImageFormat] = &[Self::Jpeg, Self::Png, Self::Gif, Self::Webp];

    /// The format's media type, as the contract spells it.
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }

    /// The format whose media type is `media_type`, if the contract names one.
    pub(crate) fn from_media_type(media_type: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|format| format.media_type() == media_type)
    }

    /// The format a file's first bytes claim, if any.
    fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
            Some(Self::Png)
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some(Self::Jpeg)
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some(Self::Gif)
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP".as_slice()) {
            Some(Self::Webp)
        } else {
            None
        }
    }
}

/// An image of a format [`ImageFormat`] names, as the file's own bytes.
///
/// The loop transforms nothing: the bytes go to the provider as given.
#[derive(Clone, PartialEq, Eq)]
pub struct Image {
    format: ImageFormat,
    bytes: Vec<u8>,
}

impl Image {
    /// An image from a file's bytes. The format is read from the first
    /// bytes: PNG's eight-byte signature, JPEG's `FF D8 FF`, `GIF87a` or
    /// `GIF89a`, or `RIFF` with `WEBP` at offset 8. That is what the file
    /// claims to be, not a check that it decodes: a corrupt or truncated
    /// file with a right signature is accepted here and is the vendor's
    /// to refuse.
    pub fn new(bytes: Vec<u8>) -> Result<Image, ImageError> {
        if bytes.is_empty() {
            return Err(ImageError::Empty);
        }
        let format = ImageFormat::sniff(&bytes).ok_or(ImageError::UnknownFormat)?;
        Ok(Image { format, bytes })
    }

    /// The format the bytes' start names.
    pub fn format(&self) -> ImageFormat {
        self.format
    }

    /// The file's bytes, as given.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl fmt::Debug for Image {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Image {{ format: {:?}, bytes: {} }}",
            self.format,
            self.bytes.len()
        )
    }
}

/// An image the provider resolves from `reference`, a string the embedder
/// chose (a path, a storage key).
///
/// The loop stores the reference and never interprets it. The format is the
/// caller's declaration, not checked against anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageReference {
    format: ImageFormat,
    reference: String,
}

impl ImageReference {
    /// A reference to an image of the declared format. The reference must
    /// not be empty; whether a provider can resolve it is the provider's
    /// to answer.
    pub fn new(
        format: ImageFormat,
        reference: impl Into<String>,
    ) -> Result<ImageReference, ImageReferenceError> {
        let reference = reference.into();
        if reference.is_empty() {
            return Err(ImageReferenceError::Empty);
        }
        Ok(ImageReference { format, reference })
    }

    /// The format the caller declared.
    pub fn format(&self) -> ImageFormat {
        self.format
    }

    /// The reference, as given.
    pub fn reference(&self) -> &str {
        &self.reference
    }
}

/// Why [`ImageReference::new`] refused a reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ImageReferenceError {
    /// The reference was an empty string.
    #[error("the image reference is empty")]
    Empty,
}

/// Why [`Image::new`] refused some bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ImageError {
    /// There were no bytes.
    #[error("the image is empty")]
    Empty,
    /// The bytes do not start as any [`ImageFormat`] does.
    #[error("the image is not a JPEG, PNG, GIF or WebP file")]
    UnknownFormat,
}

/// Why [`Session::turn_with`](crate::Session::turn_with) refused its content.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ContentError {
    /// The list was empty.
    #[error("the turn carries no content")]
    Empty,
    /// An image is larger than the session allows.
    #[error("image at position {index} is {bytes} bytes; the session's max_image_bytes is {max}")]
    ImageTooLarge {
        /// The image's position in the turn's content, counting from 0.
        index: usize,
        /// Its size.
        bytes: usize,
        /// [`SessionConfig::max_image_bytes`](crate::SessionConfig::max_image_bytes).
        max: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_filler(head: &[u8]) -> Vec<u8> {
        let mut v = head.to_vec();
        v.extend_from_slice(&[0; 16]);
        v
    }

    #[test]
    fn an_image_is_known_by_how_its_bytes_start() {
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[1, 2, 3, 4]);
        webp.extend_from_slice(b"WEBP");
        for (head, format, media) in [
            (&png[..], ImageFormat::Png, "image/png"),
            (
                &[0xFF, 0xD8, 0xFF, 0xE0][..],
                ImageFormat::Jpeg,
                "image/jpeg",
            ),
            (b"GIF87a", ImageFormat::Gif, "image/gif"),
            (b"GIF89a", ImageFormat::Gif, "image/gif"),
            (&webp[..], ImageFormat::Webp, "image/webp"),
        ] {
            let image = Image::new(with_filler(head)).unwrap();
            assert_eq!(image.format(), format, "{head:?}");
            assert_eq!(image.format().media_type(), media);
        }

        assert_eq!(Image::new(vec![]).unwrap_err(), ImageError::Empty);
        let mut wave = b"RIFF".to_vec();
        wave.extend_from_slice(&[1, 2, 3, 4]);
        wave.extend_from_slice(b"WAVE");
        for (name, bytes) in [
            ("a BMP", with_filler(b"BM")),
            ("a GIF of no known version", with_filler(b"GIF88a")),
            ("RIFF WAVE", with_filler(&wave)),
            ("a cut JPEG signature", vec![0xFF, 0xD8]),
            ("a cut PNG signature", png[..7].to_vec()),
        ] {
            assert_eq!(
                Image::new(bytes).unwrap_err(),
                ImageError::UnknownFormat,
                "{name}"
            );
        }
    }

    #[test]
    fn an_image_reference_is_declared_and_never_empty() {
        let reference = ImageReference::new(ImageFormat::Webp, "k").unwrap();
        assert_eq!(reference.format(), ImageFormat::Webp);
        assert_eq!(reference.reference(), "k");
        assert_eq!(
            UserContent::from(reference).block(),
            json!({"type": "image", "source": {
                "type": "reference", "media_type": "image/webp", "reference": "k"}})
        );
        let err = ImageReference::new(ImageFormat::Png, "").unwrap_err();
        assert_eq!(err, ImageReferenceError::Empty);
        assert_eq!(err.to_string(), "the image reference is empty");
    }

    #[test]
    fn a_tool_content_is_the_contract_value() {
        assert_eq!(ToolContent::Text("a".into()).value(), json!("a"));
        let png = Image::new(vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]).unwrap();
        let blocks = ToolContent::Blocks(vec![
            UserContent::Text("a".into()),
            UserContent::Image(png),
            UserContent::ImageReference(ImageReference::new(ImageFormat::Jpeg, "k").unwrap()),
        ]);
        assert_eq!(
            blocks.value(),
            json!([
                {"type": "text", "text": "a"},
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}},
                {"type": "image", "source": {
                    "type": "reference", "media_type": "image/jpeg", "reference": "k"}},
            ])
        );
    }

    #[test]
    fn debug_output_never_carries_image_bytes() {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.resize(1 << 20, 0x41);
        let image = Image::new(bytes).unwrap();
        let shown = format!("{image:?}");
        assert!(shown.len() < 200, "{shown}");
        assert!(
            shown.contains("Png") && shown.contains("1048576"),
            "{shown}"
        );
        let shown = format!("{:?}", UserContent::Image(image.clone()));
        assert!(shown.len() < 200, "{shown}");
        assert!(shown.contains("Png"), "{shown}");
        let shown = format!(
            "{:?}",
            crate::events::Event::ToolResult {
                call: crate::events::ToolCall {
                    id: Some("c1".into()),
                    name: "img".into(),
                    arguments: "{}".into(),
                },
                content: ToolContent::Blocks(vec![UserContent::Image(image)]),
                is_error: false,
            }
        );
        assert!(shown.len() < 200, "{shown}");
        assert!(shown.contains("Png"), "{shown}");
        let shown = format!(
            "{:?}",
            ContentError::ImageTooLarge {
                index: 1,
                bytes: 1 << 20,
                max: 16
            }
        );
        assert!(shown.len() < 200 && shown.contains("1048576"), "{shown}");
    }
}
