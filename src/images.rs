use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use std::path::Path;

/// Same cap the host tools use for reference images.
pub const MAX_REF_BYTES: u64 = 35 * 1024 * 1024;

pub fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    None
}

pub fn extension_for(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "png",
    }
}

/// Reference image -> `data:` URL accepted as `input_image` content.
pub fn to_data_url(path: &Path) -> Result<String> {
    let meta = std::fs::metadata(path)
        .with_context(|| format!("reading reference image {}", path.display()))?;
    if meta.len() > MAX_REF_BYTES {
        bail!(
            "reference image {} is {} bytes; the cap is {} bytes",
            path.display(),
            meta.len(),
            MAX_REF_BYTES
        );
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading reference image {}", path.display()))?;
    let mime = sniff_mime(&bytes)
        .ok_or_else(|| anyhow!("{} is not a PNG/JPEG/WebP/GIF image", path.display()))?;
    Ok(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_magic_bytes() {
        assert_eq!(
            sniff_mime(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
            Some("image/png")
        );
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff_mime(b"GIF89a----"), Some("image/gif"));
        assert_eq!(sniff_mime(b"not an image at all"), None);
    }

    #[test]
    fn data_url_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.png");
        std::fs::write(
            &path,
            [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0],
        )
        .unwrap();
        let url = to_data_url(&path).unwrap();
        assert!(url.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn rejects_non_images() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("notes.txt");
        std::fs::write(&path, b"hello").unwrap();
        let err = to_data_url(&path).unwrap_err().to_string();
        assert!(err.contains("not a PNG/JPEG/WebP/GIF"));
    }
}
