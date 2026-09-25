# z-biz-tool-capability

矩阵的能力层（GAP-06）：图片 / 音频 / 视频处理原语。**双击打不开的一层**——
没有窗口、不发 DMG、不进 release-org，只被 pix / aigen / file / note 等工具仓以
Rust 依赖的方式调用。

详见 `z-biz-tool-lead/doc/矩阵规划/03_能力层与项目容器.md`。

## 结构

```
crates/
├── cap-img/      图片：信息 / 缩略图 / 缩放 / 旋转 / 翻转 / 裁剪 / 滤镜 / 编码 / EXIF
├── cap-audio/    音频：占位（接口待 creator 侧音频能力集中后定）
└── cap-video/    视频：占位
```

## 消费方式

代码经 **crates.io** 分发，运行时静态链接进各 app，不发运行时包：

```bash
cargo add cap-img        # 现行 0.2.0（0.1.0 首发；crates.io 只增不改）
```

各 app 的 `src-tauri/Cargo.toml`：

```toml
[dependencies]
cap-img = "0.2.0"
```

调用契约（命令名 / 参数 / 返回 / 错误码）落在 `z-biz-tool-shared/src/capability/`，
经 npm `z-biz-tool-shared` 的 `./capability` 子路径导出给 JS 侧。

## 红线（矩阵规划 03 §9）

1. 不做 JS 侧图像处理——处理只走 Rust。
2. 不在 shared 放运行时实现——shared 只放契约。
3. 不做成常驻服务或第二个 app——不发布、不打 DMG、不进 release-org。
4. 不持有文件的唯一副本——只收路径进、路径出。
5. 不调用模型——"造像素"是 aigen 的事，能力层必须保持可单测。
