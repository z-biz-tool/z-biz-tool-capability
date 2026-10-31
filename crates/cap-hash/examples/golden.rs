// 生成跨语言 golden 样本：Rust 侧（金标）产出 JSON，喂给 TS 侧逐字节比对。
//
// 用法（在 z-biz-tool-capability 根目录）：
//   cargo run -p cap-hash --example golden > ../../z-biz-tool-shared/tests/fixtures/hash-golden.json
//
// 与 `cap-hub` 的 zbiz:// 协议同一套路：Rust 是金标，
// golden.json 是「由金标实际产出」的中间物，TS 侧只单向消费它。

use cap_hash::{crc32_ieee, crc32_ieee_hex, fnv1a32, fnv1a32_hex};

fn main() {
    let inputs: Vec<(&str, &[u8])> = vec![
        ("empty", b""),
        ("ascii", b"a"),
        ("hello", b"hello world"),
        ("foobar", b"foobar"),
        ("crc-check", b"123456789"),
        ("fox", b"The quick brown fox jumps over the lazy dog"),
        ("utf8", "你好".as_bytes()),
        ("path-like", b"/tmp/a.md"),
        ("mixed", "组件 in-S → out-L".as_bytes()),
    ];

    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(
        "  \"_comment\": \"由 z-biz-tool-capability/crates/cap-hash 的 examples/golden.rs 实际生成（Rust 侧是金标）。生成方式见该文件头部。\",\n",
    );
    out.push_str("  \"_source\": \"z-biz-tool-capability/crates/cap-hash\",\n");
    out.push_str("  \"cases\": [\n");
    for (i, (name, bytes)) in inputs.iter().enumerate() {
        let comma = if i + 1 == inputs.len() { "" } else { "," };
        out.push_str(&format!(
            "    {{ \"name\": \"{}\", \"utf8\": \"{}\", \"fnv1a32\": \"{}\", \"fnv1a32Hex\": \"{}\", \"crc32\": \"{}\", \"crc32Hex\": \"{}\" }}{}\n",
            name,
            bytes.iter().map(|b| b.to_string()).collect::<Vec<_>>().join(","),
            fnv1a32(bytes),
            fnv1a32_hex(bytes),
            crc32_ieee(bytes),
            crc32_ieee_hex(bytes),
            comma
        ));
    }
    out.push_str("  ]\n}\n");
    print!("{}", out);
}
