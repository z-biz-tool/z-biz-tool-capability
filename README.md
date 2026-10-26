# z-biz-tool-capability

矩阵的能力层（GAP-06）：图片 / 音频 / 视频 / 文档处理原语 + **AI 接入原语**。
**双击打不开的一层**——没有窗口、不发 DMG、不进 release-org，只被
aigen / file / note 等工具仓以 Rust 依赖的方式调用。

详见 `z-biz-tool-lead/doc/矩阵规划/03_能力层与项目容器.md`。

## 结构

```
crates/
├── cap-img/      图片：信息 / 缩略图 / 缩放 / 旋转 / 翻转 / 裁剪 / 滤镜 / 编码 / EXIF
├── cap-audio/    音频：WAV 编解码 / 重采样 / 增益 / 淡入淡出 / 裁剪 / 拼接 / 混音
│                 / 倒放 / 声道拆分交织 / 峰值归一化 / 响度（RMS、dBFS）
├── cap-video/    视频：MP4/MOV/WebM/MKV 容器元数据探测（时长/宽高/编码/帧数/码率）
│                 + ffmpeg 可用性检测与抽帧
├── cap-doc/      文档：PDF 列页 / 文本抽取 / 元数据 / 合并 / 拆分 / 压缩 / 水印
│                 / 内嵌图提取 + EPUB / MOBI 章节解析与封面（6,448 行，2026-10-26 从 file 搬迁）
└── cap-llm/      AI 接入：OpenAI 兼容 / Anthropic / Gemini / Ollama 四协议归一，
                  chat / 流式 / 模型列表 / 连通性探测（2026-10-26 归并十套实现）
```

### 两类 crate，两种约束

前四个是**离线原语**：路径进 → 结构出（或路径出）的纯函数，不持状态、不联网，
出错返回 `Result<_, String>`，错误串里带上实际读到的值。

`cap-llm` 是**联网原语**，约束不同，见下节。这个区别必须看清楚：
能力层里放联网的东西有代价（多一个 reqwest、多一类失败模式），
所以只允许「无状态、不持凭据」的接入层进来，不允许业务编排进来。

### cap-audio 的能力边界

支持 **WAV 容器**（PCM 8/16/24/32 位、IEEE 浮点 32/64 位，含 WAVE_FORMAT_EXTENSIBLE）。
不支持 MP3 / AAC / FLAC 等压缩格式——那需要真正的解码器，不属于「原语」层；
需要时由调用方先解码成 WAV。

### cap-video 的能力边界

**能自己做的**：容器元数据。MP4/MOV（ISO BMFF）与 Matroska/WebM（EBML）
零依赖、零解码——时长、宽高、编码、帧数、码率全部取自容器头。

**必须借外力的**：解码与抽帧。纯 Rust 没有可用的全格式视频解码器，
所以做的是把 ffmpeg 的**可用性检测**与**失败原因**做扎实：没有 ffmpeg 时
返回一句可执行的报错，而不是空白的属性面板。

⚠️ `ffmpeg_available()` 的语义是「**真的跑得起来**」而不是「文件存在」。
本机就有一个反例：ffmpeg 文件齐全、可执行位正常，但缺 `libx265.215.dylib`，
一 spawn 就 dyld 报错。只查文件存在的话，这个函数会撒谎报「可用」。

`cap-audio` 与 `cap-video` **刻意零外部依赖**：容器/音频解析是纯字节计算，
零依赖换来「每条能力都能被单测覆盖」——不需要 ffmpeg、不需要网络。

### cap-doc 的能力边界

从 `z-biz-tool-file/src-tauri/src/` 的七个模块原样搬迁（2026-10-26），
语义零漂移，只做了三件事：删 11 处 `#[tauri::command]`、把 `path_guard` 换成
`paths` 归一化、测试脚手架内移。

**在的**（纯计算，无外部进程）：
`pdf/ops`（列页/合并/拆分）、`pdf/text`（文本抽取/元数据）、`pdf/compress`（重编码压缩）、
`pdf/watermark`（水印）、`pdf/image`（内嵌图提取）、`pdf/font`（ToUnicode 映射）、
`ebook`（EPUB/MOBI 章节与封面）。

**不在的**（刻意留在 file）：
`office.rs`（调 LibreOffice 外部进程）、`ocr.rs`（调 tesseract）、
`convert.rs`（md→html/pdf/epub/mobi 的导出工序）。
它们不是「路径进结构出」的原语，而是**外部工具的编排**；进能力层会把
工具依赖传染给所有下游——一个只想解析 PDF 的产品不该被迫依赖系统里装了 LibreOffice。

⚠️ **路径守卫不在这层。** `paths::input` 只查「文件存在」，`paths::output` 只做
`&str → PathBuf`。读黑名单 / 写黑名单 / `..` 拒绝属于**调用方**策略：
file 的 `path_guard` 在 `doc_cmd.rs` 里前置执行（与 `cap-img` 的约定一致）。
搬迁时这是最容易出事的一处——原实现把守卫写在解析函数内部，跟着函数搬走就会
**静默丢守卫**。因此守卫测试也一并留在 file（`doc_cmd::guard_tests`），不进本 crate。

### cap-llm 的能力边界

2026-10-26 归并。此前家族里有**十套**独立的 LLM 接入，合计 12,898 行：

| 仓 | 原文件 | 行数 |
|---|---|---|
| aigen | `src-tauri/src/ai_client.rs` | 3,227 |
| db | `src-tauri/src/report/ai.rs` | 2,808 |
| note | `src-tauri/src/commands.rs` | 1,863 |
| kb | `src-tauri/src/rag.rs` | 1,192 |
| pet | `src-tauri/src/ai.rs` | 1,171 |
| worker | `src-tauri/src/agent_session.rs` | 755 |
| shared | `src/ai/clients.ts` | 644 |
| mind | `src/services/ai.ts` | 451 |
| terminal | `src/services/aiClient.ts` | 381 |
| file | `src-tauri/src/ai.rs` | 247 |

**在的**：provider 地址归一（容忍 base URL 带或不带 `/v1`、误粘完整 endpoint）、
四种协议的鉴权与报文构造、响应解析、SSE 流式解码、错误分类、密钥脱敏、
模型列表、连通性探测。

**不在的**：
- **提示词**。「你是简洁的文件摘要助手」属于 file，「按目录生成报告」属于 db ——
  提示词是产品差异，不是基础设施。
- **密钥存储**。本 crate 只在内存里持有调用方传入的 `api_key`，不读配置文件、
  不落盘、不打印。file 存 `app_config_dir/llm.json`，note 存 `secrets.rs`，
  各仓自己管。
- **业务编排**。任务队列、重试策略、并发、费用记账。
- **图片/视频生成**。aigen 的 `generate_image_api` 是异步任务轮询协议，
  与 chat 完全不同。

⚠️ **三个「别退回旧写法」的约定**（每条都对应一个真实踩过的坑）：

1. **错误文本必须过 `redact` + `truncate_chars`**。前者防 key 泄露（有些网关会把
   `Authorization` 回显进错误体；Gemini 的 key 更是在 URL 里，reqwest 的错误
   Display 会带完整 URL）；后者防中文截断 panic —— file 旧实现用
   `&text[..text.len().min(300)]`，错误体是中文时切在字符中间，**报错误的过程自己崩了**。
2. **流式必须用 `Utf8LineDecoder`**。手写 `from_utf8_lossy` 会让中文跨 chunk 出现方块，
   且只在大模型吐长回答时偶发，本地测不出来。
3. **不静默回落**。provider 不认识就报错，不偷偷按 OpenAI 发。配置写 A、实际打 B
   是最难查的一类故障（shared 仓 01/A-11 记录过这个病）。

可单测性靠「协议层与传输层分离」保证：`wire`（报文构造/解析）与 `sse`（解码）
完全不碰网络，68 条单测零网络、零 mock。

## 消费方式

代码经 **crates.io** 分发，运行时静态链接进各 app，不发运行时包：

```bash
cargo add cap-img      # 0.3.0（0.1.0/0.2.0 为旧版；crates.io 只增不改）
cargo add cap-audio    # 0.1.0（2026-10-20 首发）
cargo add cap-video    # 0.1.0（2026-10-20 首发）
```

> 三个 crate 均已上架 crates.io。版本号**不可覆盖、不可删除**（只能 yank 撤下，
> 但版本号永久占用）⇒ 发错了只能 bump 新版本。

各 app 的 `src-tauri/Cargo.toml`：

```toml
[dependencies]
cap-img = "0.3.0"
```

> ⚠️ **消费方的依赖号还没全部跟上**（实测调用面，2026-10-20）：
>
> | 消费方 | 现在写的 | 0.3.0 的行为收紧对它的影响 |
> | --- | --- | --- |
> | aigen | `"0.2.0"` | **无** —— 只用 `decode_data_url` / `encode_data_url` / `ext_for_mime` / `mime_for_ext` / `sniff_format`（字节级原语，0.3.0 未动） |
> | note | `"0.2.0"` | **无** —— 只用 `decode_data_url` / `encode_data_url` |
> | remote | `"0.2.0"` | **无** —— 只用 `encode_frame_jpeg` |
> | file | `"0.1.0"` | **有** —— 唯一用到变换族的仓：`save_bytes` / `export` / `resize` / `rotate` / `flip` / `crop` / `apply_filter` 全在 0.3.0 的改动面上 |
>
> 也就是说：前三个升到 0.3.0 是**零行为变化**（纯版本号跟进），
> file 升 0.3.0 会**改变产出格式**（目标扩展名决定格式），需显式改依赖号 + 同步 lock
> + 复跑 file 的测试。**本轮未做**（跨仓行为变更，需单独一轮）。
> file 继续用 0.1.0 也完全可用 —— 0.1.0 仍在 crates.io 上。

调用契约（命令名 / 参数 / 返回 / 错误码）落在 `z-biz-tool-shared/src/capability/`，
经 npm `z-biz-tool-shared` 的 `./capability` 子路径导出给 JS 侧。

### cap-img 的输出格式规则（0.3.0 起）

**文件名不许说谎**：

- `save_bytes` / `export`：`format` 参数与目标扩展名必须一致，否则**拒绝**；
- `resize` / `rotate` / `flip` / `crop` / `apply_filter`：**目标扩展名**决定输出格式
  （此前按源图扩展名 ⇒ 目标叫 `.jpg` 的文件里装着 PNG 字节）；
- 目标扩展名认不出来 ⇒ **报错**，不悄悄写成 PNG；
- 目标路径**无扩展名**时，才回落源图格式（调用方明确要无后缀文件）。

写入 JPEG 时会先降到 RGB —— JPEG 没有 alpha 通道。

## 红线（矩阵规划 03 §9）

1. 不做 JS 侧图像处理——处理只走 Rust。
2. 不在 shared 放运行时实现——shared 只放契约。
3. 不做成常驻服务或第二个 app——不发布、不打 DMG、不进 release-org。
4. 不持有文件的唯一副本——只收路径进、路径出。
5. **不碰模型输出**——"造像素/造文本"是 aigen / pet 的事，能力层只提供
   传输与解析原语。
   `cap-llm` 是这条的**明确例外**：它联网，但不持密钥、不落盘、不缓存、
   不决定提示词、不做业务编排，仍然是无状态原语。它的可单测性由
   「协议层/传输层分离 + 脱网测试」保证，而不是靠「不联网」。
