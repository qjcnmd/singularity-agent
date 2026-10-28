//! `view_image` 工具：把磁盘上的图片文件作为视觉输入返回；文本文件由 `read` 负责。

use std::fs::File;
use std::path::Path;

use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::registry::{ABORTED_MESSAGE, ToolExecution, error_result};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ViewImageArgs {
    pub(crate) path: String,
}

pub(crate) fn spec() -> singularity_model::ModelToolSchema {
    singularity_model::ModelToolSchema {
        name: "view_image".to_string(),
        description: "View an image file (PNG, JPEG, WebP, GIF first frame, BMP). The image is returned as visual input, not text.".to_string(),
        parameters_schema: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the image file (relative or absolute)" },
            },
            "required": ["path"],
            "additionalProperties": false,
        }),
    }
}

pub(crate) fn execute(args: &ViewImageArgs, cwd: &Path, signal: &CancellationToken) -> ToolExecution {
    if signal.is_cancelled() {
        return error_result(ABORTED_MESSAGE);
    }
    let full_path = cwd.join(&args.path);
    let file = match File::open(&full_path) {
        Ok(file) => file,
        Err(error) => return error_result(format!("Could not read file: {}. {error}", args.path)),
    };
    let image = match crate::image::InputImage::read(&full_path, file) {
        Ok(image) => image,
        Err(error) => return error_result(error),
    };
    if signal.is_cancelled() {
        return error_result(ABORTED_MESSAGE);
    }
    let mut result = ToolExecution::text(format!(
        "Read image: {} ({} × {})",
        args.path, image.attachment.width, image.attachment.height
    ));
    result.images.push(image);
    result
}
