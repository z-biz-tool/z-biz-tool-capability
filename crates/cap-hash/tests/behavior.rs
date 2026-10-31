//! cap-hash 的行为测试：对外承诺的口径逐条钉死。
//!
//! 这些用例不只是「跑通」——它们是跨语言一致性的凭据：
//! TS 侧的孪生实现（z-biz-tool-shared/src/hash）必须算出同样的数。
//! golden 样本见 z-biz-tool-shared/tests/fixtures/hash-golden.json。

use cap_hash::{crc32_ieee, crc32_ieee_hex, fnv1a32, fnv1a32_hex, fnv1a32_str, Crc32};

#[test]
fn fnv1a32_参考向量() {
    assert_eq!(fnv1a32(b""), 0x811c_9dc5);
    assert_eq!(fnv1a32(b"a"), 0xe40c_292c);
    assert_eq!(fnv1a32(b"foobar"), 0xbf9c_f968);
    assert_eq!(fnv1a32(b"hello world"), 0xd58b_3fa7);
}

#[test]
fn crc32_ieee_参考向量() {
    assert_eq!(crc32_ieee(b"123456789"), 0xCBF4_3926);
    assert_eq!(crc32_ieee(b""), 0);
    assert_eq!(
        crc32_ieee(b"The quick brown fox jumps over the lazy dog"),
        0x414F_A339
    );
}

#[test]
fn 十六进制补零小写() {
    assert_eq!(fnv1a32_hex(b"a"), "e40c292c");
    assert_eq!(fnv1a32_hex(b""), "811c9dc5");
    assert_eq!(crc32_ieee_hex(b"123456789"), "cbf43926");
}

#[test]
fn 字符串按_utf8_字节() {
    assert_eq!(fnv1a32_str("abc"), fnv1a32(b"abc"));
    assert_eq!(fnv1a32_str(""), fnv1a32(b""));
    // 中文走 UTF-8 字节，不是 UTF-16 码元 —— 与 TS 侧统一后的口径一致
    assert_eq!(
        fnv1a32_str("你好"),
        fnv1a32(&[0xe4, 0xbd, 0xa0, 0xe5, 0xa5, 0xbd])
    );
}

#[test]
fn 流式与一次性结果一致() {
    let data: Vec<u8> = (0..=255u8).collect();
    for chunk_size in [1usize, 3, 16, 64, 255, 256] {
        let mut c = Crc32::new();
        for chunk in data.chunks(chunk_size) {
            c.update(chunk);
        }
        assert_eq!(
            c.finalize(),
            crc32_ieee(&data),
            "分块大小 {chunk_size} 时不一致"
        );
    }
}

#[test]
fn 状态可克隆可重复取结果() {
    let mut c = Crc32::new();
    c.update(b"123456789");
    let first = c.finalize();
    assert_eq!(first, c.finalize());
    let mut d = c;
    d.update(b"more");
    assert_ne!(d.finalize(), first);
}

#[test]
fn 改动一个字节结果就变() {
    let a = fnv1a32(b"content-hash");
    let b = fnv1a32(b"content-hasg");
    assert_ne!(a, b);
    assert_ne!(crc32_ieee(b"content-hash"), crc32_ieee(b"content-hasg"));
}
