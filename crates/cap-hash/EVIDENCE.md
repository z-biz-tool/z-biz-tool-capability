# cap-hash 的准入证据

> 这个 crate 为什么能进能力层，依据是什么，接了谁，还欠什么。
> 判据来源：`z-biz-tool-lead/doc/矩阵规划/03_能力层与项目容器.md`（准入三约束 + 红线五条）。
> 证据里的每一处都当场量过，行号可复核；**推测一律写成推测**。

## 一、它解决的重复：家族里有四份同一件事的抄本

| 仓 | 位置 | 算法 | 语言 | 规模 | 状态 |
|----|------|------|------|------|------|
| cpu | `src/core/hash.ts` | FNV-1a 32（UTF-16 码元口径） | TS | 40 行 | 2026-10-31 由仓内 4 份合一 |
| cpu（此前） | `src/core/sim.ts` / `src/courses/judging.ts` / `src/workshop/index.ts` / `src/courses/manifest.ts` | FNV-1a 32 | TS | 4 × ~12 行 | 已收口到 `src/core/hash.ts` |
| terminal | `src/utils/sftpEditPath.ts:19-27` | FNV-1a 32（UTF-16 码元口径） | TS | 9 行 | 未迁移 |
| mind | `src/engine/emmx/zip.ts:39-53` | CRC-32/IEEE（表驱动） | TS | 15 行 | 未迁移 |
| file | `src-tauri/src/commands.rs:1534-1563` | CRC-32/IEEE（表驱动） | Rust | 30 行 | **已迁移到本 crate** |

同一常量在两处以上出现：FNV-1a 的 `0x811c9dc5` / `0x01000193` 出现在 cpu 与 terminal；
CRC-32 的反射多项式 `0xEDB88320` 出现在 mind 与 file。
更值得记一笔的是 **cpu 一个仓内曾有四份**——同一套常量抄了四遍，
其中一份的乘法口径还与其他三份不同（`h * PRIME` vs `Math.imul`），
在同一份内容包上算出了两个不同的哈希（实测 `e8234e8` vs `b2df3925`）。
这类漂移不靠 review 能拦住，只能靠「一份实现 + 一处调用」。

## 二、三条准入判据逐条核对

| 判据 | 结论 | 依据 |
|------|------|------|
| 同一件事在 ≥2 个仓被重复实现 | ✅ | FNV-1a：cpu + terminal；CRC-32：mind + file（且 cpu 仓内 4 份） |
| 能被写成纯函数并单测 | ✅ | 字节进 → 数值出；零依赖；`cargo test -p cap-hash` 12 条（7 行为 + 5 文档）全过 |
| 不持状态、不持文件唯一副本 | ✅ | 输入是字节切片；`Crc32` 只是 4 字节的计算进度，不缓存文件、不落盘 |

## 三、五条红线逐条核对

1. **不做 JS 侧图像处理——处理只走 Rust**：本 crate 就是 Rust 侧。
   TS 侧仍需要一份同口径实现（前端无法同步调 Rust），
   这是家族既有先例（`cap-hub` 的 `zbiz://` 协议在 TS 侧有 `shared/src/protocol/deeplink.ts` 孪生），
   处理方式相同：**一份 canonical + golden 样本逐字节钉死**。
2. **shared 只放契约**：哈希的 TS 孪生是**实现**，严格说与这条有张力。
   与 deeplink 先例一致处理，并在 `z-biz-tool-shared/src/hash/index.ts` 头部写明理由：
   跨语言必须逐位一致，只有把实现收在一处 + golden 钉死才不会漂。
   （本条为**已知例外**，不是"没有例外"。）
3. **不做常驻服务或第二个 app**：不发布、不打 DMG、不进 release-org —— ✅
4. **不持有文件的唯一副本**：只收路径/字节进、值出 —— ✅
5. **不碰模型输出**：不造像素、不造文本 —— ✅

## 四、口径声明：标准 FNV-1a 按字节，不是 UTF-16 码元

本 crate 采用**标准** FNV-1a 32（offset `0x811c9dc5`、prime `0x01000193`、按字节混合）
与**标准** CRC-32/IEEE（反射多项式 `0xEDB88320`、init/final 异或 `0xFFFFFFFF`）。

⚠️ **TS 侧的历史实现按 UTF-16 码元混合 FNV，不是字节。**
ASCII 输入两者完全相同；**含非 ASCII 的输入结果不同**。
影响面（已核）：

- `terminal` 的 `hash8`：用于远端文件名截断后的去重后缀 —— 含中文的路径名会得到不同的后缀。
  它是本地去重键，不是跨端协议，变了不影响正确性。
- `cpu` 的 `contentHash` / `designHash`：输入是元件 id / 类型 / 参数 JSON。
  其中 `note` 元件的文本参数可以是中文 ⇒ 含中文参数的自定义组件哈希会变，
  表现为**工坊里多出一个版本**（旧版本仍在，不丢东西）。

结论：这是**已知且可接受**的口径变更，迁移动作见第六节。

## 五、谁已经接上了

- **file（Rust）**：2026-10-31 迁移完成。
  `src-tauri/src/commands.rs` 的 `calculate_file_hash` 的 `crc32` 分支不再自建查表，
  改用 `cap_hash::Crc32` 流式喂字节。
  验证：`cargo test crc32 --lib` 两条**钉死已知值**的用例通过
  （小文件已知值 + 32MB 文件流式），说明迁移前后输出逐位相同。

## 六、还欠什么（不掩饰）

1. **TS 侧三处尚未切换**（cpu / terminal / mind）。
   切换步骤：待 `z-biz-tool-shared` 发布含 `src/hash` 的版本后，
   各仓把本地实现换成 `import { fnv1a32, crc32 } from "z-biz-tool-shared/hash"`，
   并用 `z-biz-tool-shared/tests/fixtures/hash-golden.json` 钉住口径。
   **本机 npm 未登录（`npm whoami` 报 ENEEDAUTH）**，发版这一步本轮做不了 ——
   这是阻塞点，不是设计选择。
2. **crates.io 未发布**：`cap-hash` 0.1.0 名字仍敞着。
   crates.io 的版本号不可覆盖、不可删除，发错只能 bump；
   本轮不具备发布凭据，因此只落仓内 + path 依赖。

## 七、刻意不做的事

- **md5 / sha1 / sha256 不进这个 crate**：file 那边用的是成熟库（`md5`、`sha1`、`sha2`），
  不是"重复实现"，搬进能力层只会多一层包装。
- **不做文件哈希命令**：`hash_file(path)` 这类编排（开文件、流式喂、错误文案）
  属于调用方策略，留在 file；能力层只提供字节进、数值出的原语。
