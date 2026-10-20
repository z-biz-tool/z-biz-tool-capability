#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""cap-img 变异验证：把实现改回错误写法，看测试网是否真的变红。

**为什么要有这个脚本**：测试全绿不等于测试咬得住。
2026-10-20 逐条读 cap-img 的 23 条断言时发现，其中 4 条只验「调用成功 / 文件存在」，
把 rotate 实现成恒等、flip 实现成恒等、grayscale 实现成原样返回，全都能过。
补强断言之后（23 → 36 条），本脚本用**变异**证明这 36 条真的钉得住行为。

**判据纪律**（沿用 cap-audio/cap-video 两轮的教训）：

1. **以进程退出码为唯一判据**。不 grep 输出里的 `test result:` 行 ——
   上一版脚本曾 grep 到 lib 目标那行 `0 passed ... ok`，把 10 个变异全读成"通过"。
2. **编译失败不算咬住**。变异让代码编译不过 ⇒ 说明这个变异没打在语义上，
   单独归类，不计入"被抓住"。
3. **首轮没咬住要逐条查根因**：是「判据不敏感」还是「变异没写进去」，
   两者的处置完全不同（前者补断言，后者改脚本）。
4. **还原用内存字节快照 + sha256**，不用 git —— 工作区有未提交改动，
   用 git checkout 会把本轮的真改动一起冲掉。
5. **等价变异如实说明**。语义等价的变异不算缺陷，但必须写清楚为什么等价。

用法：
    python3 scripts/mutation_cap_img.py            # 跑全部
    python3 scripts/mutation_cap_img.py --only M03 # 只跑某条
    python3 scripts/mutation_cap_img.py --json out.json
"""

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
TARGET = REPO / "crates" / "cap-img" / "src" / "lib.rs"

# ---------------------------------------------------------------------------
# 变异清单
#
# 每条 = (编号, 说明, 原文, 替换文, 期望)
# 期望 "caught" 表示这条变异**应该**被测试网抓住（红）。
# ---------------------------------------------------------------------------
MUTATIONS = [
    (
        "M01",
        "crop 越界检查用 unchecked_add（回注本轮修掉的溢出缺陷）",
        "let (x_end, y_end) = match (x.checked_add(w), y.checked_add(h)) {",
        "let (x_end, y_end) = match (Some(x + w), Some(y + h)) {",
        "caught",
    ),
    (
        "M02",
        "crop 越界判断改成 > 之外还允许贴边以外（把 >= 与 > 弄反）",
        "if x_end > img.width() || y_end > img.height() {",
        "if x_end >= img.width() || y_end >= img.height() {",
        "caught",
    ),
    (
        "M03",
        "EXIF 的 IFD0 越界检查退回只比起点（回注本轮修掉的越界 panic）",
        "if ifd0_offset + 2 > data.len() {",
        "if ifd0_offset >= data.len() {",
        "caught",
    ),
    (
        "M04",
        "输出格式改回按源图扩展名决定（回注本轮修掉的说谎扩展名）",
        "let image_format = match dest.extension() {",
        "let image_format = match Some(std::ffi::OsStr::new(\"png\")) {",
        "caught",
    ),
    (
        "M05",
        "export 不再校验 format 与目标扩展名是否一致",
        "    if !want.contains(&ext) {\n        return Err(format!(\n            \"目标扩展名与所选格式不符: {} vs {}\",\n            dest.display(),",
        "    if false && !want.contains(&ext) {\n        return Err(format!(\n            \"目标扩展名与所选格式不符: {} vs {}\",\n            dest.display(),",
        "caught",
    ),
    (
        "M06",
        "rotate 的 90 度改成 180 度（方向转错）",
        "        90 => img.rotate90(),",
        "        90 => img.rotate180(),",
        "caught",
    ),
    (
        "M07",
        "flip 的横竖两个分支写反",
        "    let flipped = if horizontal { img.fliph() } else { img.flipv() };",
        "    let flipped = if horizontal { img.flipv() } else { img.fliph() };",
        "caught",
    ),
    (
        "M08",
        "crop 忽略请求区域，恒裁左上角 1x1",
        "    let cropped = img.crop_imm(x, y, w, h);",
        "    let cropped = img.crop_imm(0, 0, 1, 1);",
        "caught",
    ),
    (
        "M09",
        "grayscale 滤镜改成原样返回（恒等）",
        '        "grayscale" => Ok(DynamicImage::ImageLuma8(img.to_luma8())),',
        '        "grayscale" => Ok(img.clone()),',
        "caught",
    ),
    (
        "M10",
        "invert 滤镜不再取反",
        "                pixel[0] = 255 - pixel[0];",
        "                pixel[0] = pixel[0];",
        "caught",
    ),
    (
        "M11",
        "sepia 滤镜的系数全改成 0（滤镜等于清空颜色）",
        "                let new_r = (r * 0.393 + g * 0.769 + b * 0.189).min(255.0) as u8;",
        "                let new_r = 0u8;",
        "caught",
    ),
    (
        "M12",
        "brightness 滤镜系数 1.3 改成 1.0（等于不做）",
        "                pixel[0] = (pixel[0] as f32 * 1.3).min(255.0) as u8;",
        "                pixel[0] = (pixel[0] as f32 * 1.0).min(255.0) as u8;",
        "caught",
    ),
    (
        "M13",
        "thumbnail 不再缩放，原图直出",
        "    let thumb = img.thumbnail(size, size);",
        "    let thumb = img.clone();",
        "caught",
    ),
    (
        "M14",
        "thumbnail 的 max_size=0 回落值从 200 改成 0",
        "    let size = if max_size == 0 { 200 } else { max_size };",
        "    let size = if max_size == 0 { 0 } else { max_size };",
        "caught",
    ),
    (
        "M15",
        "thumbnail 返回的 width/height 字段不取实际尺寸（恒填 0）",
        "    let (width, height) = thumb.dimensions();",
        "    let (width, height) = (0u32, 0u32);",
        "caught",
    ),
    (
        "M16",
        "info 的 size 字段恒填 0",
        "    let size = metadata.len();",
        "    let size = 0u64;",
        "caught",
    ),
    (
        "M17",
        "info 的 has_alpha 恒填 false",
        "    let has_alpha = img.color().has_alpha();",
        "    let has_alpha = false;",
        "caught",
    ),
    (
        "M18",
        "EXIF 的 tag 映射去掉 Model（0x0110）",
        '            0x0110 => ("Model", read_u32(&data[entry + 8..entry + 12]) as usize),',
        '            0x0111 => ("Model", read_u32(&data[entry + 8..entry + 12]) as usize),',
        "caught",
    ),
    (
        "M19",
        "sniff_format 的 PNG 魔数少一个字节",
        '    if bytes.starts_with(b"\\x89PNG\\r\\n\\x1a\\n") {',
        '    if bytes.starts_with(b"\\x89PNG\\r\\n") {',
        "caught",
    ),
    (
        "M20",
        "sniff_format 的 WebP 长度门槛从 12 放宽到 4",
        "    } else if bytes.len() >= 12 && &bytes[..4] == b\"RIFF\" && &bytes[8..12] == b\"WEBP\" {",
        "    } else if bytes.len() >= 4 && &bytes[..4] == b\"RIFF\" {",
        "caught",
    ),
    (
        "M21",
        "decode_data_url 不再要求 meta 里有 base64 标记",
        "    if !meta.to_ascii_lowercase().contains(\"base64\") {",
        "    if false {",
        "caught",
    ),
    (
        "M22",
        "decode_data_url 的 mime 不再小写化",
        "        .to_ascii_lowercase();\n    let bytes = base64_decode_padded(payload.trim()).ok()?;",
        "        .to_string();\n    let bytes = base64_decode_padded(payload.trim()).ok()?;",
        "caught",
    ),
    (
        "M23",
        "base64 解码不再补齐 padding（无 padding 的 payload 读不出）",
        "    while !s.len().is_multiple_of(4) {",
        "    while false {",
        "caught",
    ),
    (
        "M24",
        "ext_for_mime 的 jpeg 分支返回 jpeg 而非 jpg",
        '            "jpeg" | "jpg" => "jpg".to_string(),',
        '            "jpeg" | "jpg" => "jpeg".to_string(),',
        "caught",
    ),
    (
        "M25",
        "encode_frame_jpeg 的缩放高度改成向下取整",
        "        let new_h = ((height as f64) * (max_width as f64 / width as f64)).ceil() as u32;",
        "        let new_h = ((height as f64) * (max_width as f64 / width as f64)).floor() as u32;",
        "caught",
    ),
    (
        "M26",
        "encode_frame_jpeg 的长度校验被去掉（长度不符不再报错）",
        "    if rgba_raw.len() != expected {",
        "    if false {",
        "caught",
    ),
    (
        "M27",
        "encode_frame_jpeg 的 max_width 语义改成「总是缩放」（已更窄也放大）",
        "    if max_width != 0 && width > max_width {",
        "    if max_width != 0 {",
        "caught",
    ),
    (
        "M28",
        "detect_format 不再把 jpeg 归一成 jpg",
        '        "jpg" | "jpeg" => "jpg".to_string(),',
        '        "jpg" => "jpg".to_string(),\n        "jpeg" => "jpeg".to_string(),',
        "caught",
    ),
    (
        "M29",
        "parse_format 不再支持 tif",
        '        "tiff" | "tif" => Ok(ImageFormat::Tiff),',
        '        "tiff" => Ok(ImageFormat::Tiff),',
        "caught",
    ),
    (
        "M30",
        "save_bytes 不再校验扩展名与格式是否一致（回注说谎文件名）",
        "    if !want.contains(&ext) {\n        return Err(format!(\n            \"目标扩展名与所选格式不符: {} vs {}\",\n            dest_path.as_ref().display(),",
        "    if false && !want.contains(&ext) {\n        return Err(format!(\n            \"目标扩展名与所选格式不符: {} vs {}\",\n            dest_path.as_ref().display(),",
        "caught",
    ),
    (
        "M31",
        "export 的 quality 不再夹到 1..=100（等价变异：编码器自己会夹）",
        # 为什么等价：image crate 的 JPEG 编码器自己就把 quality 夹到 1..=100。
        # 实测（64x64 噪声图）：q=0 与 q=1 同为 1462 字节，q=10 → 1655，
        # q=100 / 101 / 255 同为 15934 字节 ⇒ 去掉 export 里的 clamp 行为不变。
        # 这**不是**测试缺口，也不该为了"让变异变红"去改实现
        # —— 那属于为了指标而改产品行为。
        # 另有一条独立用例 `export_quality_changes_the_output_size` 用产物字节数
        # 钉住 quality 真的生效（q=10 vs q=100 差 10 倍），那条是真判据。
        "    let quality = quality.clamp(1, 100);",
        "    let quality = quality;",
        "equivalent",
    ),
    (
        "M32",
        "滤镜的未知名字不再报错（悄悄按原图返回）",
        "    let filtered = filter_impl(&img, filter_name)?;",
        "    let filtered = filter_impl(&img, \"grayscale\").unwrap_or_else(|_| img.clone());",
        "caught",
    ),
    (
        "M33",
        "rotate 不再拒绝非法角度（45 度也照转）",
        "        _ => return Err(format!(\"不支持的角度: {}，仅支持 90/180/270\", degrees)),",
        "        _ => img.rotate180(),",
        "caught",
    ),
    (
        "M34",
        "crop 不再拒绝越界区域（照裁不误）",
        "    if x_end > img.width() || y_end > img.height() {",
        "    if false {",
        "caught",
    ),
]


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def run_cargo_test() -> tuple[int, str]:
    """跑 cap-img 的测试，返回 (退出码, 输出尾部)。

    **退出码是唯一判据**：不解析输出里的 test result 行。
    """
    env = dict(os.environ)
    env["CARGO_TERM_COLOR"] = "never"
    proc = subprocess.run(
        ["cargo", "test", "-p", "cap-img"],
        cwd=REPO,
        env=env,
        capture_output=True,
        text=True,
        timeout=900,
    )
    out = proc.stdout + proc.stderr
    return proc.returncode, out


def classify(rc: int, out: str) -> str:
    """把一次运行归类。"""
    if rc == 0:
        return "survived"          # 变异后测试仍全绿 ⇒ 没咬住
    if "error[E" in out or "error: could not compile" in out:
        return "compile_error"     # 编译失败 ⇒ 变异没打在语义上，不算咬住
    return "caught"                # 测试红了 ⇒ 咬住了


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--only", help="只跑指定编号，如 M03")
    ap.add_argument("--json", help="把结果写成 JSON")
    args = ap.parse_args()

    if not TARGET.is_file():
        print(f"找不到目标文件: {TARGET}", file=sys.stderr)
        return 2

    original = TARGET.read_bytes()
    original_sha = sha256(original)
    workdir = Path(tempfile.mkdtemp(prefix="cap-img-mut-"))
    results = []

    # 先确认基线是绿的（否则"变异后变红"没有意义）
    rc, out = run_cargo_test()
    if rc != 0:
        print("基线测试就不绿，变异验证无从谈起：", file=sys.stderr)
        print(out[-3000:], file=sys.stderr)
        shutil.rmtree(workdir, ignore_errors=True)
        return 2
    print(f"基线: cargo test -p cap-img 通过 (rc=0)")

    selected = [m for m in MUTATIONS if not args.only or m[0] == args.only]
    if args.only and not selected:
        print(f"没有编号为 {args.only} 的变异", file=sys.stderr)
        shutil.rmtree(workdir, ignore_errors=True)
        return 2

    try:
        for mid, desc, old, new, expect in selected:
            text = original.decode("utf-8")
            count = text.count(old)
            if count != 1:
                results.append({
                    "id": mid, "desc": desc, "expect": expect,
                    "status": "anchor_miss", "detail": f"锚点在源文件里出现 {count} 次",
                })
                print(f"  {mid:<4} ⚠️  锚点未命中或命中多次({count}) —— 变异没写进去")
                continue

            mutated = text.replace(old, new, 1)
            TARGET.write_text(mutated, encoding="utf-8")
            try:
                rc, out = run_cargo_test()
            finally:
                # 还原：用内存快照，不动 git
                TARGET.write_bytes(original)

            status = classify(rc, out)
            # 还原校验：必须与原始字节完全一致
            restored = TARGET.read_bytes()
            if sha256(restored) != original_sha:
                results.append({
                    "id": mid, "desc": desc, "expect": expect,
                    "status": "restore_failed", "detail": "还原后字节与原始不一致",
                })
                print(f"  {mid:<4} ❌ 还原失败！")
                continue

            # 抓一条红测试的名字，便于人工核对
            red = ""
            if status == "caught":
                for line in out.splitlines():
                    if line.strip().startswith("test ") and "FAILED" in line:
                        red = line.strip()
                        break
                    if line.strip().startswith("---- ") and line.strip().endswith(" stdout ----"):
                        red = line.strip()
                        break

            results.append({
                "id": mid, "desc": desc, "expect": expect,
                "status": status, "rc": rc, "red_hint": red,
            })
            mark = {"caught": "✅ 咬住", "survived": "❌ 存活", "compile_error": "⚠️  编译失败"}.get(status, status)
            print(f"  {mid:<4} {mark}  {desc}")
    finally:
        TARGET.write_bytes(original)
        shutil.rmtree(workdir, ignore_errors=True)

    # 汇总
    caught = sum(1 for r in results if r["status"] == "caught")
    survived = [r for r in results if r["status"] == "survived"]
    compile_err = [r for r in results if r["status"] == "compile_error"]
    anchor_miss = [r for r in results if r["status"] == "anchor_miss"]
    # 等价变异：语义上与原实现无可观察差异，**不是测试缺口**。
    # 它既不计入"咬住"，也不计入"存活缺口"——单列，如实说明为什么等价。
    equivalent = [r for r in results if r["status"] == "survived" and r.get("expect") == "equivalent"]

    print()
    print(
        f"合计 {len(results)} 条变异：咬住 {caught} / 存活 {len(survived)}"
        f"（其中等价 {len(equivalent)}） / 编译失败 {len(compile_err)} / 锚点未命中 {len(anchor_miss)}"
    )
    if survived:
        real_gaps = [r for r in survived if r.get("expect") != "equivalent"]
        if real_gaps:
            print("存活（测试网有缺口，必须逐条查根因）：")
            for r in real_gaps:
                print(f"  - {r['id']} {r['desc']}")
        if equivalent:
            print("存活但属**等价变异**（语义无可观察差异，不是缺口）：")
            for r in equivalent:
                print(f"  - {r['id']} {r['desc']}")
    if compile_err:
        print("编译失败（变异没打在语义上，单独归类，不计入咬住）：")
        for r in compile_err:
            print(f"  - {r['id']} {r['desc']}")
    if anchor_miss:
        print("锚点未命中（脚本问题，不是测试问题）：")
        for r in anchor_miss:
            print(f"  - {r['id']} {r['desc']} —— {r.get('detail')}")

    if args.json:
        Path(args.json).write_text(
            json.dumps(results, ensure_ascii=False, indent=2), encoding="utf-8"
        )
        print(f"结果已写入 {args.json}")

    # 退出码：等价变异不算失败（它不是缺口）；真缺口 / 编译失败 / 锚点未命中都算失败
    real_gaps = [r for r in survived if r.get("expect") != "equivalent"]
    return 0 if (not real_gaps and not compile_err and not anchor_miss) else 1


if __name__ == "__main__":
    sys.exit(main())
