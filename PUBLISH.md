# 能力层发布计划（crates.io + npm）

> 目标：把 `cap-*` 系列 crate 推上 crates.io 全网可装，把 `z-biz-tool-shared` 推上 npm，
> 然后把各消费方从 git+tag 依赖切到正式版本依赖。
> 状态：**进行中**，每步做完打勾并记录实测数字。

---

## 背景

- 本仓（z-biz-tool-capability）已建好：workspace + `cap-img`（10 个公开函数，11 例集成测试）+ `cap-audio`/`cap-video` 占位。
- CI 三平台（macOS / Ubuntu / Windows）cargo test + clippy 已全绿，tag `v0.1.0`、`v0.1.1` 已打。
- 消费方现状：`z-biz-tool-file` 用 git 依赖 `cap-img = { git = "...", tag = "v0.1.1" }`；
  `z-biz-tool-shared` 0.1.1 已加 `./capability` 导出但**未发 npm**。
- 名字可用性已核验：crates.io 上 `cap-img` / `cap-audio` / `cap-video` 均 404（可注册）。

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

---

## 阶段 3 · 消费方切换到 crates.io 版本依赖

- [ ] `z-biz-tool-file/src-tauri/Cargo.toml`：
      `cap-img = { git = "...", tag = "v0.1.1" }` → `cap-img = "0.1.1"`
- [ ] 重写 Cargo.lock（`cargo update -p cap-img`）并核对来源已变成 crates.io
- [ ] 门禁：`cargo test`（此前 181/181）、`npm run typecheck`（0 错）、`npm run build`
- [ ] commit + push

**状态：** [ ] 未开始

---

## 阶段 4 · 发布 z-biz-tool-shared@0.1.1 到 npm

- [ ] `npm login`（或 `NPM_TOKEN` 环境变量；token 若存于 lead `004_重要秘钥/`，需用户明示授权后才用）
- [ ] 发布前核对：`package.json` version=0.1.1、`exports["./capability"]` 在、`files:["dist"]`、先 `npm run build`
- [ ] `npm publish`（该包 `publishConfig.access=public`，已确认）
- [ ] 验证：`npm view z-biz-tool-shared version` → `0.1.1`；干净目录 `npm i z-biz-tool-shared@0.1.1` 后 `import ... from 'z-biz-tool-shared/capability'` 可解析
- [ ] 消费方（file / pet 等）package.json 升到 `^0.1.1`

**状态：** [ ] 未开始

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
- [ ] 阶段 4（npm 发 shared@0.1.1）与阶段 5 仍未开始

---

## 阶段 5 · 收尾

- [ ] capability 仓 README 加安装说明（`cargo add cap-img`）
- [ ] lead 侧回写：能力层分发方式从"提案"改为"已落地（crates.io cap-img + npm shared@0.1.1）"
- [ ] 后续 `cap-audio` / `cap-video` 有实体代码时按本计划同流程发 0.1.0

**状态：** [ ] 未开始

---

## 不做的事（明确边界）

1. 不 `cargo publish` 未打过 CI 绿灯的版本。
2. 不在没有用户授权的情况下使用任何 token（crates.io / npm 皆然）。
3. 不删除/覆盖已发布版本；发错只能 bump 新版本 + yank 旧版本。
4. 占位 crate（cap-audio/cap-video 空壳）**不发布**——crates.io 不允许纯占位包浪费名字，等有实现再说。
