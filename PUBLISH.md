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

**状态：** [ ] 未开始

---

## 阶段 2 · 发布 cap-img 到 crates.io

### 2.1 发布前检查单

- [ ] `Cargo.toml` 版本号 = 想发的版本（当前 `0.1.0`，如已用过 0.1.0 则 bump 到 `0.1.1` 与 tag 对齐）
- [ ] `license` / `repository` / `description` / `keywords` / `categories` 字段齐全（crates.io 强制要求 description + license 或 license-file）
- [ ] 本仓 CI 三平台绿（tag 对应 commit）
- [ ] `cargo package -p cap-img --list` 只包含 src/tests/Cargo.toml/README，无秘钥无 target

### 2.2 发布命令

```bash
cd z-biz-tool-capability
cargo package -p cap-img          # 先打包装箱，验证能编
cargo publish -p cap-img          # 真发布
```

### 2.3 验证

- [ ] `https://crates.io/crates/cap-img` 页面可见
- [ ] 干净目录 `cargo new probe && cd probe` 里 `cargo add cap-img` 能拉下来

**状态：** [ ] 未开始

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
