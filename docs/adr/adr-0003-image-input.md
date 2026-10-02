# 图片输入与持久快照

日期：2026-10-02。状态：已采用。

图片从工作台粘贴、拖入、选择和 Agent 的 `read` 进入同一图片准备过程。根据实际字节识别并解码，PNG、JPEG、WebP 保留原字节，GIF 首帧和 BMP 转为 PNG；解码使用 `image` 库。文字与有序图片组成完整输入，复用现有排队、插话、编辑、取消和未消费输入归还机制。

已发送或工具读取的像素归 Agent 会话层所有，保存于 `sessions/images/<sessionId>/<imageId>`。先原子保存图片，再追加引用消息，随后发布事件。历史、事件和模型消息各在其边界转换；模型请求和预览按需读取快照，原文件变化不影响历史。归档只移动 JSONL，像素目录按任务身份保持稳定。图片保存成功、日志追加失败时可能留下未引用快照；保留它们以免误删部分追加可能已经引用的文件，不自动回收用户数据。

模型消息沿用文字字段并增加有序图片 URL；当前入口只需文字后跟图片，没有任意穿插内容的需求。Chat 的工具结果只能包含文字，编码器在完整工具结果组之后补图片 user 消息；Responses 用 `function_call_output.output` 内容数组直接携带图片。内部事实仍保留原始角色和配对。上下文剪枝保留图片块，摘要沿用同一视觉投影；图片尺寸仅用于压力估计，实测用量继续来自 provider usage。

配置模型均按支持文本和图像使用，图片经过相应协议编码后直接发送。

工作台将完整文字、图片草稿保存在 IndexedDB，保存新记录成功后迁移旧文字草稿；普通恢复过程不增加提示。输入缩略图与历史预览共用呈现。采用 DSH 的缩略图、点击大图和图片内移除操作，保存机制按本项目的单页面与会话所有权实现。

参考 [DSH 持久附件](https://github.com/deepseek-ai/deepseek-harness/blob/master/docs/subsystems/attachment.md)、[DSH 图片输入呈现](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/client/ui-attachment/src/client/ComposerAttachments.tsx)、[Gemini 文件读取](https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/utils/fileUtils.ts)、[Cline Chat 工具图片投影](https://github.com/cline/cline/blob/main/sdk/packages/llms/src/providers/middleware/split-tool-images.ts)和 [Responses 工具输出类型](https://github.com/openai/openai-python/blob/main/src/openai/types/responses/response_function_call_output_item_list_param.py)。采用输入与历史共用快照、显示与模型载荷分离及协议正确投影；需要跨任务共享图片、输出媒体或适配具体模型的尺寸预算时再评估存储和模型内容结构。
