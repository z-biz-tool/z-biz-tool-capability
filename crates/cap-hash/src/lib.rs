//! 哈希原语：FNV-1a 32 位 + CRC-32/IEEE。
//!
//! # 这个 crate 为什么存在
//!
//! 家族里有四处各自抄写的哈希实现，同一套常量、同一个算法、不同的写法：
//!
//! | 仓 | 位置 | 算法 | 语言 |
//! |----|------|------|------|
//! | cpu | `src/core/hash.ts` | FNV-1a 32 | TS |
//! | terminal | `src/utils/sftpEditPath.ts` | FNV-1a 32 | TS |
//! | mind | `src/engine/emmx/zip.ts` | CRC-32/IEEE | TS |
//! | file | `src-tauri/src/commands.rs` | CRC-32/IEEE | Rust |
//!
//! 证据与准入判据见同目录 `EVIDENCE.md`。
//!
//! # 口径（跨语言必须逐位一致）
//!
//! 哈希是「拿去做内容标识」的数：同一份内容必须得到同一个数，
//! 不同语言算出来不一样就会变成「同一个文件在两个仓里对不上」。
//! 因此本 crate 的口径是**标准 FNV-1a 32 / 标准 CRC-32/IEEE**，不是变种：
//!
//! - FNV-1a 32：offset basis `0x811c9dc5`、prime `0x01000193`、**按字节**混合；
//! - CRC-32/IEEE：反射多项式 `0xEDB88320`、init `0xFFFFFFFF`、末尾异或 `0xFFFFFFFF`。
//!
//! ⚠️ **TS 侧的历史实现按 UTF-16 码元混合 FNV，不是字节。**
//! ASCII 输入两者相同；含中文的输入两者不同（本 crate = UTF-8 字节口径）。
//! TS 侧统一后的口径与本 crate 一致，golden 样本在
//! `z-biz-tool-shared/tests/fixtures/hash-golden.json`，改一边必须改另一边。
//!
//! # 边界
//!
//! 纯计算：不读文件、不联网、不持状态、不做编排。
//! 需要「算文件哈希」的调用方自己打开文件流式喂进来（见 [`Crc32`]）。

/// FNV-1a 32 位的 offset basis
pub const FNV1A_32_OFFSET: u32 = 0x811c_9dc5;
/// FNV-1a 32 位的 prime
pub const FNV1A_32_PRIME: u32 = 0x0100_0193;

/// CRC-32/IEEE 的反射多项式
pub const CRC32_IEEE_POLY: u32 = 0xEDB8_8320;

/// 编译期算出的 256 项查表（逐位展开的结果，值与逐位实现逐位相同）
const fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ CRC32_IEEE_POLY
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

const CRC32_TABLE: [u32; 256] = crc32_table();

/// FNV-1a 32 位哈希：字节进 → 数值出。
///
/// 标准 FNV-1a：对每个字节做 `h ^= byte; h = h * PRIME`（32 位环绕）。
///
/// # Examples
///
/// ```
/// assert_eq!(cap_hash::fnv1a32(b""), 0x811c_9dc5);
/// assert_eq!(cap_hash::fnv1a32(b"a"), 0xe40c_292c);
/// assert_eq!(cap_hash::fnv1a32(b"foobar"), 0xbf9c_f968);
/// ```
pub fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut h = FNV1A_32_OFFSET;
    for &b in bytes {
        h ^= b as u32;
        h = h.wrapping_mul(FNV1A_32_PRIME);
    }
    h
}

/// FNV-1a 32 位哈希（字符串按 UTF-8 字节）。
///
/// # Examples
///
/// ```
/// assert_eq!(cap_hash::fnv1a32_str("abc"), cap_hash::fnv1a32(b"abc"));
/// ```
pub fn fnv1a32_str(text: &str) -> u32 {
    fnv1a32(text.as_bytes())
}

/// FNV-1a 32 位哈希的 8 位小写十六进制（补零）。
///
/// # Examples
///
/// ```
/// assert_eq!(cap_hash::fnv1a32_hex(b"a"), "e40c292c");
/// ```
pub fn fnv1a32_hex(bytes: &[u8]) -> String {
    format!("{:08x}", fnv1a32(bytes))
}

/// CRC-32/IEEE 哈希：字节进 → 数值出。
///
/// 标准 CRC-32（IEEE 802.3 多项式，反射实现）。
///
/// # Examples
///
/// ```
/// assert_eq!(cap_hash::crc32_ieee(b"123456789"), 0xCBF4_3926);
/// ```
pub fn crc32_ieee(bytes: &[u8]) -> u32 {
    let mut crc = Crc32::new();
    crc.update(bytes);
    crc.finalize()
}

/// CRC-32/IEEE 哈希的 8 位小写十六进制（补零）。
pub fn crc32_ieee_hex(bytes: &[u8]) -> String {
    format!("{:08x}", crc32_ieee(bytes))
}

/// 流式 CRC-32/IEEE：恒定内存，算大文件不撑爆内存。
///
/// 大文件（GB 级）不要用 [`crc32_ieee`]——它要求整块字节进内存；
/// 读一块喂一块即可。
///
/// # Examples
///
/// ```
/// let mut c = cap_hash::Crc32::new();
/// c.update(b"1234");
/// c.update(b"56789");
/// assert_eq!(c.finalize(), 0xCBF4_3926);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crc32 {
    state: u32,
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32 {
    /// 新建一个流式 CRC 状态
    pub fn new() -> Self {
        Crc32 { state: 0xFFFF_FFFF }
    }

    /// 继续喂一段字节
    pub fn update(&mut self, bytes: &[u8]) {
        let mut c = self.state;
        for &b in bytes {
            c = CRC32_TABLE[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
        }
        self.state = c;
    }

    /// 当前结果（可以多次取，结果不变）
    pub fn finalize(&self) -> u32 {
        self.state ^ 0xFFFF_FFFF
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a32_matches_reference_vectors() {
        assert_eq!(fnv1a32(b""), 0x811c_9dc5);
        assert_eq!(fnv1a32(b"a"), 0xe40c_292c);
        assert_eq!(fnv1a32(b"foobar"), 0xbf9c_f968);
    }

    #[test]
    fn crc32_matches_reference_vector() {
        // CRC-32/IEEE 的标准检验值
        assert_eq!(crc32_ieee(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32_ieee(b""), 0);
    }

    #[test]
    fn streaming_crc_equals_oneshot() {
        let data: Vec<u8> = (0..=255u8).collect();
        let mut c = Crc32::new();
        for chunk in data.chunks(7) {
            c.update(chunk);
        }
        assert_eq!(c.finalize(), crc32_ieee(&data));
    }

    #[test]
    fn hex_is_padded_lowercase() {
        assert_eq!(fnv1a32_hex(b"a"), "e40c292c");
        assert_eq!(crc32_ieee_hex(b"123456789"), "cbf43926");
        assert_eq!(fnv1a32_hex(b""), "811c9dc5");
    }

    #[test]
    fn fnv1a32_str_matches_utf8_bytes() {
        assert_eq!(fnv1a32_str("abc"), fnv1a32(b"abc"));
        assert_eq!(fnv1a32_str("你好"), fnv1a32("你好".as_bytes()));
    }
}
