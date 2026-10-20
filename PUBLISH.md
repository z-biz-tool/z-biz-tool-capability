# 能力层发布计划（crates.io + npm）

> 目标：把 `cap-*` 系列 crate 推上 crates.io 全网可装，把 `z-biz-tool-shared` 推上 npm，
> 然后把各消费方从 git+tag 依赖切到正式版本依赖。
> 状态：**已收官**（2026-09-25）：crates.io cap-img 0.1.0/0.2.0 + npm shared 0.1.1 均已发布，
> 4 个 Rust 消费方全走 registry。每步的实测数字留在下面各阶段。
> **2026-10-20 追加发布**（用户授权「可以，发布」）：crates.io 现有 **cap-img 0.1.0/0.2.0/0.3.0
> + cap-audio 0.1.0 + cap-video 0.1.0** —— 三个 crate 的名字**全部认领到手**，
> 窗口期问题消解。实录见阶段 2.5 与 `doc/优化方案/08_本轮深度优化实施记录.md` §8.10。

---

## 背景

- 本仓（z-biz-tool-capability）已建好：workspace + `cap-img`（10 个公开函数，11 例集成测试）+ `cap-audio`/`cap-video` 占位。
- CI 三平台（macOS / Ubuntu / Windows）cargo test + clippy 已全绿，tag `v0.1.0`、`v0.1.1` 已打。
- 消费方现状：`z-biz-tool-file` 用 git 依赖 `cap-img = { git = "...", tag = "v0.1.1" }`；
  `z-biz-tool-shared` 0.1.1 已加 `./capability` 导出但**未发 npm**。
- 名字可用性已核验：crates.io 上 `cap-img` / `cap-audio` / `cap-video` ~~均 404（可注册）~~。
  > **2026-10-20 订正**：这句话**现在只对了一半** —— `cap-img` 已发布 0.2.0（HTTP 200），
  > `cap-audio` / `cap-video` 仍 404（**仍可注册，名字仍敞着**）。
  > 2026-10-20 复测，与 2026-10-04 初测一致。
  >
  > **2026-10-20 18:59 终态**：三个名字**全部认领到手**，HTTP 全 200 ——
  > `cap-img` 0.3.0（新增）/`cap-audio` 0.1.0（新增）/`cap-video` 0.1.0（新增）。
  > 「名字仍敞着」这个窗口期问题**到此消解**：crate 名是先到先得且永久占用，
  > 现在这三个名字谁都抢不走了。

---

## 阶段 0 · 注册 crates.io 账号（人工，约 3 分钟）

1. 打开 <https://crates.io> → 右上角 **Log in with GitHub**（用 z-biz-tool 组织的 GitHub 账号登录，避免个人/组织混用）。
2. 首次登录会要求填一个邮箱（用于发布通知，不会公开）→ 提交后去邮箱点验证链接。
3. 登录后点右上角头像 → **Account** → **API Tokens** 标签 → **New Token**。
   - Token name 填 `z-biz-tool-local`（随意）。
   - **Scopes 不要勾任何东西**（默认只读+发布权限即可，最小授权）。
   - 点 Generate → 立即复制 token（**只显示这一次**）。
4. 把 token 交给本地执行者（见阶段 1），或自己在终端跑 `cargo login <token>`。

> ⚠️ crates.io 一旦发布**不可覆盖、不可删除**（只能 yank 撤下但版本号永久占用）。
> 所以发布前必须确认版本号与内容，见阶段 2 的检查单。

---

## 阶段 1 · 本地 cargo 登录（1 条命令）

```bash
cargo login <token>
```

- 写入 `~/.cargo/credentials.toml`，之后 `cargo publish` 自动带上。
- 注意：本机 rsproxy 镜像**只影响下载**，`cargo publish` 永远发往官方 crates.io，不受镜像影响。
- 验证：`cargo login --list` 能看到已存的 token 条目。

**状态：** [x] 完成（2026-09-25）：token 已存入 `~/.cargo/credentials.toml`；并按组织规矩登记源台账
`ceo/003_组织管理/keys/KEYS.md` §crates-io（指纹 `b5b10acb7958`），经 `distribute_keys.py --apply`
分发到 `z-biz-tool-lead/004_重要秘钥/keys.md`，`--check` 漂移 0。
注：crates.io `/api/v1/me` 被站点保护挡 curl（403 与 token 无关），存活以真发布为准。

---

## 阶段 2 · 发布 cap-img 到 crates.io

### 2.1 发布前检查单

- [x] `Cargo.toml` 版本号 = `0.1.0`（首次发布，与 tag v0.1.0 对齐；元数据已补 repository/keywords/categories，commit `88c43c5`）
- [x] `license` / `repository` / `description` / `keywords` / `categories` 字段齐全
- [x] 本仓 CI 三平台绿（tag 对应 commit）
- [x] `cargo package -p cap-img --list` 只包含 6 个文件（Cargo.toml/lock/vcs_info + src/tests），无秘钥无 target
- [x] `cargo package -p cap-img` 打包后编译通过（17.77s）
- [x] `cargo publish -p cap-img --dry-run` 通过：Packaged 54.3KiB（16.2KiB 压缩）+ 校验编译 2.87s

### 2.2 发布命令

```bash
cd z-biz-tool-capability
cargo package -p cap-img          # 先打包装箱，验证能编
cargo publish -p cap-img          # 真发布
```

### 2.3 验证

- [x] `https://crates.io/crates/cap-img` 页面可见
- [x] 干净目录 `cargo new probe` + `cargo add cap-img` → 拉取 + 编译通过（BUILD_OK，实测）
- [x] **0.1.0 与 0.2.0 均已发布**（0.2.0 增补 data URL / 魔数嗅探 / RGBA 帧编码，测试 11→23，clippy 0）

**状态：** [x] 完成（2026-09-25）：`cargo publish -p cap-img` 发布 **0.1.0** 与 **0.2.0**，
干净项目 `cargo add cap-img` 实测可拉取并编译。

> **2026-10-20 追加：0.3.0 已备好但**未发布**。**
> 本轮 cap-img 行为收紧（输出格式改由目标扩展名决定、`export` 新增格式/扩展名一致性校验、
> 目标扩展名认不出时报错、修两个 panic）⇒ 对调用方是**破坏性变更**。
> crates.io 的版本号**不可覆盖、不可删除**，0.x 语义下 minor 即 breaking
> ⇒ 不能复用 0.2.0，必须 bump 到 **0.3.0**。
> 仓内 `crates/cap-img/Cargo.toml` 已是 0.3.0；**crates.io 上的现行版本仍是 0.2.0**。
> 发布前检查单照阶段 2 走（版本号 = 0.3.0、`cargo package` 只含预期文件）。
> 影响面已核：aigen / note / remote 用的全是**字节级原语**（decode_data_url /
> encode_frame_jpeg / sniff_format / mime 映射），**未受影响**；
> 只有 file 仓用到变换族，且它 lock 的是 0.1.0，升级需显式改依赖号。
> 详见 `doc/优化方案/08_本轮深度优化实施记录.md` §8.5。
>
> **↑ 以上已被 2026-10-20 18:59 的实际发布取代，见下面「阶段 2.5」。**

---

## 阶段 2.5 · 2026-10-20 追加发布：cap-img 0.3.0 + cap-audio 0.1.0 + cap-video 0.1.0

> 触发：用户授权「可以，发布」。三个 crate 一次发布，**crate 名全部认领到手**。
> 本节的每个数字都来自**当次实跑**，命令见 `doc/优化方案/08` §8.10。

### 2.5.1 发布前检查单（逐项实测，全部过）

| 检查项 | 实测结果 |
| --- | --- |
| 版本号不与 crates.io 既有版本冲突 | cap-img 0.3.0（既有 0.1.0/0.2.0）、cap-audio 0.1.0、cap-video 0.1.0 |
| 元数据齐全 | `license` / `repository` / `homepage` / `description` / `keywords` / `categories` 三者皆齐 |
| 本地 fmt | `cargo fmt --all --check` **rc=0** |
| 本地 clippy | `cargo clippy --all-targets --all-features -- -D warnings` **rc=0** |
| 本地测试 | `cargo test --workspace` **94 passed / 0 failed**（cap-audio 29 / cap-img 40 / cap-video 3+22） |
| **三平台 CI** | GitHub Actions run `39402819882`（commit `e69081d`）**三个 job 全 success**（macOS / Ubuntu / Windows 逐 job 核过） |
| 打包文件清单 | `cargo package --list`：cap-img 6 / cap-audio 8 / cap-video 9 个文件，**无秘钥无 target** |
| 干跑 | `cargo publish --dry-run` 三个 crate **全 rc=0**（含从 tarball 的校验编译） |
| 工作区状态 | `HEAD == origin/main == e69081d`，`git status --porcelain -uall` 为空 |

### 2.5.2 第一次发布被 400 拒（cap-video）

```
error: failed to publish cap-video v0.1.0 to registry at https://crates.io
Caused by:
  the remote server responded with an error (status 400 Bad Request):
  expected at most 5 keywords per crate
```

crates.io 硬限制 **keywords 最多 5 个**，cap-video 原写了 6 个
（`video` / `mp4` / `webm` / `metadata` / `thumbnail` / `ffmpeg`）。

关键性质：该校验发生在**上传前** ⇒ **crate 未建成、名字未被占**
（复测 `GET /api/v1/crates/cap-video` 仍 HTTP 404）。
⇒ 失败**不消耗名字**，修掉即可重发。

修法：去掉最偏实现细节的 `ffmpeg`，保留 5 个能力词
（commit `1d3109a`）。改完**重跑三平台 CI 并全绿**（run `39413304769`）才重发。

### 2.5.3 发布结果（crates.io API 实测）

| crate | 版本 | created_at | yanked | 当前状态 |
| --- | --- | --- | --- | --- |
| `cap-img` | **0.3.0** | 2026-10-20T10:59:45Z | false | 新增，nowest |
| `cap-audio` | **0.1.0** | 2026-10-20T10:59:50Z | false | 新增，**名字已认领**（原 404） |
| `cap-video` | **0.1.0** | 2026-10-20T11:15:35Z | false | 新增，**名字已认领**（原 404） |

`cap-img` 0.1.0 / 0.2.0 仍在线、未 yank（不可删除、不可覆盖，只能 yank 撤下）。

### 2.5.4 干净目录安装验证（阶段 2.3 口径）

`/tmp/probe_pub` 全新 `cargo init` → `cargo add cap-img cap-audio cap-video` →
拉到 **0.3.0 / 0.1.0 / 0.1.0**，lock 三个条目 source 均为
`registry+https://github.com/rust-lang/crates.io-index`（非 git 依赖），
编译通过并**实跑**：

```
[cap-img]   sniff_format(png) = Some("png")
[cap-img]   mime_for_ext(jpg) = image/jpeg
[cap-img]   encode_data_url -> data:image/png;base64,iVBORw0K...  decode 回环 = true
[cap-audio] encode_wav(I16) -> 52 bytes, RIFF魔数 = [82, 73, 70, 70]
[cap-audio] probe_wav 解析 = true      rms_dbfs = -8.5194
[cap-video] sniff_container(mp4) = Some("mp4")
[cap-video] ffmpeg_available() = false   （本机 ffmpeg 缺 dylib ⇒ 如 README 所述会报错而非撒谎）
```

### 2.5.5 本阶段**未做**的

- **未推 git tag** —— crates.io 发布不依赖 tag；且 cap-audio / cap-video 的 0.1.0
  与仓内既有 tag `v0.1.0`（cap-img 时代）**撞名**，需要先定 tag 命名约定
  （例如 `cap-audio-v0.1.0` / `cap-video-v0.1.0` / `cap-img-v0.3.0`），
  属拍板事项，未擅自定。
- **未升级消费方依赖号** —— aigen / note / remote 写的是 `cap-img = "0.2.0"`，
  file 写的是 `"0.1.0"`。实测调用面：前三个只用字节级原语（0.3.0 未动这些）
  ⇒ 升级是零行为变化；**file 是唯一用到变换族的仓**，升级会改变产出格式，
  需单独一轮。0.1.0 仍在 crates.io 上，file 继续用 0.1.0 **完全可用**。
- **未在发布物里带 LICENSE 文件** —— 与已发布的 0.2.0 保持一致（同样是 6 个文件、
  无 LICENSE）；SPDX `license = "MIT"` 已在元数据里。要补的话三个 crate 目录各放一份 LICENSE。

---

## 阶段 3 · 消费方切换到 crates.io 版本依赖

- [x] `z-biz-tool-file/src-tauri/Cargo.toml`：`cap-img = "0.1.0"`（commit `2b631a2`）
- [x] Cargo.lock 来源已核对 = `registry+https://github.com/rust-lang/crates.io-index`，
      checksum `2c3fd1e5…`
- [x] 门禁：cargo test / typecheck / build 绿（当时实测，见该 commit）
- [x] commit + push

**状态：** [x] 完成（2026-09-25）。另核：aigen / note / remote 三仓 lock 的 cap-img 0.2.0
来源同为 crates.io，checksum 一致（`ca8e4802…`）——**4 个消费方全部走 registry，零 git 依赖**。

---

## 阶段 4 · 发布 z-biz-tool-shared@0.1.1 到 npm

- [x] `npm run build` + `typecheck` 双绿；`dist/capability/{types.js,types.d.ts}` 在
- [x] `npm publish --dry-run` → 192 文件 / 170.4 kB / sha `71c0c666`
- [x] 用 lead 台账 npm token（`004_重要秘钥/keys.md` §npm-token）发往 registry.npmjs.org
- [x] 验证：`npm view` 从 0.1.0 变 0.1.1（**发布→可见耗时约 4 分钟**，npm 有 processing 队列，
      期间 `notarget` 属正常，不是失败）；干净目录安装后
      `import 'z-biz-tool-shared/capability'` 实测可解析
- [~] 消费方版本号：**不逐仓改**。8 个消费仓写的是 `^0.1.0`，caret 已覆盖 0.1.1，
      真正开始 import `./capability` 时再 `npm i z-biz-tool-shared@^0.1.1` 即可，
      现在批量改只换来 8 份 lockfile 抖动

**状态：** [x] 完成（2026-09-25）：`0.1.1` 已在 npmjs，干净目录安装 + 子路径导入实测通过。

---

## 阶段 3.5 · 消费方接入 cap-img 0.2.0（2026-09-25 追加）

盘点结论：全工作区【可直接迁移】0 处、【需先扩 cap-img】6 处（aigen/note/remote 各 2），
【不迁移】约 18 处（权限边界业务、只播放、外部二进制、纯前端 canvas）。cap-audio/cap-video
本期零迁移对象，维持空壳。

- [x] **aigen** `55f81e9`：sniff/mime/encode 下沉 cap-img；信任边界（symlink/8MB/白名单）原样保留；
      decode_data_url 变薄委托，3 个调用方零改动。cargo 141/141、typecheck 0、build OK
- [x] **note** `10bcaf2`：data URI 拆分 + base64 编解码换 cap-img，删 33 行手写 base64；
      mime↔ext 映射留本仓（svg 单列是 cap-img 没有的）。行为差异：含非法字符的 payload
      由「静默丢弃坏字符后落盘」改为直接报错。cargo 51/51、typecheck 0、build OK
- [x] **remote** `7dff546`：**顺带修一个真 bug** —— `ecb895e`（2026-09-06）删掉降通道步骤后，
      截屏把 4 通道数据声明成 Rgb8，image 0.25 每次 panic，至 2026-09-25 无修复，功能全程不可用。
      改走 `encode_frame_jpeg`；image/base64 直接依赖清零（base64 降为 dev-dep）。
      本仓首次有测试 3 例，**故障注入实证**：注入旧实现 → 2 例红（panic 在 capture.rs:67），
      恢复 → 3/3 绿。cargo 3/3、typecheck 0、build OK
- [x] 阶段 4（npm 发 shared@0.1.1）与阶段 5 已完成，见下

---

## 阶段 5 · 收尾

- [x] capability 仓 README 加安装说明（`cargo add cap-img`，并改掉还写着 git+tag 的消费方式）
- [x] lead 侧回写：`03_能力层与项目容器.md` §4.2 用法行改成 crates.io 正式版，
      §11 表新增"分发方式已落地"一行（含 4 消费方 lock 来源、npm 子路径实测、18 处不迁盘点）
- [x] bootstrap `manifest.json` 登记 `z-biz-tool-capability`（00 §175 的 16 仓记 15 漂移）——
      2026-09-25 以 `type: capability` 登记，**登记前先核了 `should_sync` 分支**：
      `sync.py:59` 与 `release_org.py:97` 都是严格 `type == "product"`，无 enum 校验、新取值不会被拒；
      实测 sync 默认模式 / `release-org --plan` 均回「没匹配任何仓」，`status` clean、`audit` 零新增 issue、
      `--license-only` 回「所有仓都已有 LICENSE」（该模式刻意绕过 `should_sync`，但文件存在即跳过）
- [x] **许可证全工作区统一为 MIT**（2026-09-25 用户拍板"所有统一一下"）：
      本仓 `LICENSE` 原是 Apache-2.0（Initial commit `4284a57` 带入，全 16 仓唯一异类）→ 换成家族统一文本
      （与其余 15 仓 md5 同 `5cfa635d…`，`Copyright (c) 2026 z-biz-tool`）；
      顺带补齐元数据：**15 个 `package.json` 的 `license`**（14 个缺失字段 + `pet` 原为 `ISC`）、
      **12 个 app 的 `src-tauri/Cargo.toml` `license = "MIT"`**（3 个 cap-* 本来就是 MIT；
      `remote-server` 本就是 MIT 只补了末尾换行；`worker/agent-proxy/dist` 是构建产物不改）。
      理由：crates.io 上 cap-img 0.1.0/0.2.0 已写死 MIT 且不可覆盖（反向改要 0.2.1 且新旧声明打架），
      Apache 唯一实质优势的专利条款对内部库空转，且多 NOTICE/变更声明义务。
      复盘点：LICENSE 文件 **16/16 MIT 且 md5 同为一族**（db/sys 原本只是末尾缺换行，已补）、
      package.json **16/16 MIT**（16 个源文件，另有 1 个 `dist/package.json` 是构建产物不计）、
      Cargo.toml **15/16 带 `license = "MIT"`**（差的 1 个是 capability
      workspace 根、无 `[package]` 段故不适用；16 = 12 个 app + 3 个 cap-* + 1 个根）；
      `cargo metadata --no-deps` 12/12 OK（改动只在 `[package]` 段加一行，无编译面影响）
- [x] 后续 `cap-audio` / `cap-video` 有实体代码时按本计划同流程发 0.1.0
      —— **条件已满足并执行（2026-10-20）**：两个 crate 已于 2026-10-05 从 4 行空壳
      写成真能力（cap-audio 1160 行 src / 29 条测试，cap-video 1202 行 src / 25 条测试），
      当日按本计划同流程发布 **0.1.0**（见阶段 2.5）。

**状态：** [x] 全部落地（2026-09-25），含 manifest 以 `type: capability` 登记并实测四条脚本、
许可证全工作区统一 MIT。
**2026-10-20 追加**：`cap-img` 0.3.0 + `cap-audio` 0.1.0 + `cap-video` 0.1.0 已发布，
三个 crate 名全部认领（见阶段 2.5）。

---

## 不做的事（明确边界）

1. 不 `cargo publish` 未打过 CI 绿灯的版本。
2. 不在没有用户授权的情况下使用任何 token（crates.io / npm 皆然）。
3. 不删除/覆盖已发布版本；发错只能 bump 新版本 + yank 旧版本。
4. **空壳 crate 不发布**——crates.io 不允许纯占位包浪费名字，等有实现再说。
   > 这条**原则不变**，但触发条件已变：cap-audio / cap-video 已于 2026-10-05 从 4 行空壳
   > 写成真能力（1160 / 1202 行 src + 各自测试网），**不再是空壳** ⇒
   > 2026-10-20 按阶段 2.5 正常发布 0.1.0。
   > 「名字先到先得、永久占用」正是当初不能急着发空壳的原因：
   > 先把能力做完再占名，名占下来才不亏。

