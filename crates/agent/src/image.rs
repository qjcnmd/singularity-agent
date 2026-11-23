use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::Arc;

use base64::{Engine, engine::general_purpose::STANDARD};
use image::{ImageFormat, ImageReader, Limits};
use protocol::{ImageAttachment, ImageUpload};

const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const MAX_IMAGE_DIMENSION: u32 = 16_384;
const MAX_DECODE_ALLOC_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct InputImage {
    pub attachment: ImageAttachment,
    bytes: Arc<[u8]>,
}

#[derive(Debug)]
pub struct SavedImage {
    pub(crate) directory: std::path::PathBuf,
    pub(crate) attachment: ImageAttachment,
}

impl SavedImage {
    pub fn data_url(&self) -> std::io::Result<String> {
        load_image(&self.directory, &self.attachment)
    }
}

impl InputImage {
    pub fn data_url(&self) -> String {
        data_url(&self.attachment.mime_type, &self.bytes)
    }

    pub(crate) fn upload(upload: ImageUpload) -> Result<Self, String> {
        let (header, data) = upload.data_url.split_once(',').ok_or("图片数据无效。")?;
        if !header.starts_with("data:") || !header.ends_with(";base64") {
            return Err("图片必须使用 Base64 data URL。".into());
        }
        Self::from_base64(upload.name, data)
    }

    pub(crate) fn from_base64(name: String, data: &str) -> Result<Self, String> {
        if data.len() > MAX_IMAGE_BYTES * 4 / 3 + 4 {
            return Err("图片大小超过 20 MB 上限。".into());
        }
        let bytes = STANDARD.decode(data).map_err(|_| "图片 Base64 数据无效。")?;
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err("图片大小超过 20 MB 上限。".into());
        }
        let format = image::guess_format(&bytes).map_err(|_| "无法识别图片格式。")?;
        Self::prepare(name, bytes, format)
    }

    pub(crate) fn read(path: &Path, mut reader: impl Read) -> Result<Self, String> {
        const HEADER_PROBE_BYTES: u64 = 64;
        let mut bytes = Vec::new();
        reader
            .by_ref()
            .take(HEADER_PROBE_BYTES)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("无法读取图片：{error}"))?;
        let format = image::guess_format(&bytes).map_err(|_| "无法识别图片格式。")?;
        let remaining = (MAX_IMAGE_BYTES + 1 - bytes.len()) as u64;
        reader
            .take(remaining)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("无法读取图片：{error}"))?;
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err("图片文件大小超过 20 MB 上限。".into());
        }
        Self::prepare(path.file_name().unwrap_or_default().to_string_lossy().into_owned(), bytes, format)
    }

    fn prepare(name: String, mut bytes: Vec<u8>, format: ImageFormat) -> Result<Self, String> {
        let mime = match format {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::WebP => "image/webp",
            ImageFormat::Gif | ImageFormat::Bmp => "image/png",
            _ => return Err("支持 PNG、JPEG、WebP、GIF 和 BMP 图片。".into()),
        };
        let mut reader = ImageReader::with_format(Cursor::new(&bytes), format);
        let mut limits = Limits::default();
        limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
        limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
        limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);
        reader.limits(limits);
        let decoded = reader.decode().map_err(|error| format!("图片无法解码：{error}"))?;
        let (width, height) = (decoded.width(), decoded.height());
        let name = if format == ImageFormat::Gif {
            format!("{name}（首帧）")
        } else {
            name
        };
        if matches!(format, ImageFormat::Gif | ImageFormat::Bmp) {
            let mut encoded = Cursor::new(Vec::new());
            decoded
                .write_to(&mut encoded, ImageFormat::Png)
                .map_err(|error| format!("图片编码失败：{error}"))?;
            bytes = encoded.into_inner();
            if bytes.len() > MAX_IMAGE_BYTES {
                return Err("转换后的 PNG 图片大小超过 20 MB 上限。".into());
            }
        }
        Ok(Self {
            attachment: ImageAttachment {
                id: uuid::Uuid::now_v7().to_string(),
                name,
                mime_type: mime.into(),
                width,
                height,
            },
            bytes: Arc::from(bytes),
        })
    }

    pub(crate) fn save(&self, directory: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(directory)?;
        std::fs::write(directory.join(&self.attachment.id), &self.bytes)
    }
}

fn data_url(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", STANDARD.encode(bytes))
}

pub(crate) fn load_image(directory: &Path, attachment: &ImageAttachment) -> std::io::Result<String> {
    uuid::Uuid::parse_str(&attachment.id)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let path = directory.join(&attachment.id);
    let bytes = std::fs::read(&path).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("无法读取图片 {}（{}）：{error}", attachment.name, path.display()),
        )
    })?;
    Ok(data_url(&attachment.mime_type, &bytes))
}
