//! 图片入口共用的识别与快照：上传和 read 只在取得字节的方式上不同。

use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::Arc;

use base64::{Engine, engine::general_purpose::STANDARD};
use image::{ImageFormat, ImageReader};
use singularity_protocol::{ImageAttachment, ImageUpload};

/// 已校验的图片字节；控制请求复制时共享像素，不复制大块数据。
#[derive(Debug, Clone, PartialEq)]
pub struct InputImage {
    pub attachment: ImageAttachment,
    bytes: Arc<Vec<u8>>,
}

impl InputImage {
    /// 排队中的图片尚未保存到历史，工作台按需读取这份已校验的内容。
    pub fn data_url(&self) -> String {
        data_url(&self.attachment.mime_type, &self.bytes)
    }

    /// 接收工作台 data URL，声明的 MIME 不作为格式依据。
    pub fn upload(upload: ImageUpload) -> Result<Self, String> {
        let (header, data) = upload.data_url.split_once(',').ok_or("图片数据无效。")?;
        if !header.starts_with("data:") || !header.ends_with(";base64") {
            return Err("图片必须使用 Base64 data URL。".into());
        }
        Self::from_base64(upload.name, data)
    }

    /// 解码外部图片的 Base64 内容，按实际字节识别格式。
    pub(crate) fn from_base64(name: String, data: &str) -> Result<Self, String> {
        let bytes = STANDARD.decode(data).map_err(|_| "图片 Base64 数据无效。")?;
        Self::prepare(name, bytes)
    }

    /// 读取一次文件内容，后续保存和模型请求使用这份快照。
    pub(crate) fn read(path: &Path, mut reader: impl Read) -> Result<Self, String> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).map_err(|error| format!("无法读取图片：{error}"))?;
        Self::prepare(path.file_name().unwrap_or_default().to_string_lossy().into_owned(), bytes)
    }

    fn prepare(name: String, mut bytes: Vec<u8>) -> Result<Self, String> {
        let format = image::guess_format(&bytes).map_err(|_| "无法识别图片格式。")?;
        let mime = match format {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::WebP => "image/webp",
            ImageFormat::Gif | ImageFormat::Bmp => "image/png",
            _ => return Err("支持 PNG、JPEG、WebP、GIF 和 BMP 图片。".into()),
        };
        let decoded = ImageReader::with_format(Cursor::new(&bytes), format)
            .decode()
            .map_err(|error| format!("图片无法解码：{error}"))?;
        let (width, height) = (decoded.width(), decoded.height());
        let name = if format == ImageFormat::Gif {
            format!("{name}（首帧）")
        } else {
            name
        };
        if matches!(format, ImageFormat::Gif | ImageFormat::Bmp) {
            let mut encoded = Cursor::new(Vec::new());
            decoded.write_to(&mut encoded, ImageFormat::Png).map_err(|error| error.to_string())?;
            bytes = encoded.into_inner();
        }
        Ok(Self {
            attachment: ImageAttachment {
                id: uuid::Uuid::now_v7().to_string(),
                name,
                mime_type: mime.into(),
                width,
                height,
            },
            bytes: Arc::new(bytes),
        })
    }

    pub(crate) fn save(&self, directory: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(directory)?;
        // 图片写完才保存消息引用；重试仍写入同一份字节。
        std::fs::write(directory.join(&self.attachment.id), self.bytes.as_slice())
    }
}

fn data_url(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", STANDARD.encode(bytes))
}

/// 从持久快照构造视觉输入；源文件后续的变化不会改变历史。
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
