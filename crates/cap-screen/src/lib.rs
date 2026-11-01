//! cap-screen — 屏幕采集原语。
//!
//! ## 为什么有这个 crate
//!
//! 截屏能力此前**私藏在 remote 仓的 `capture.rs` 里**（148 行，用 xcap 0.9.8），
//! 只截整块显示器，没有区域选框、没有窗口捕获。box 仓的「截图工具」则是
//! 另一套完全不同的实现 —— 纯前端 `getDisplayMedia` 模拟版：
//!
//! - 区域选框**没实现**（选了「区域截图」也只会截全屏）
//! - 录屏按钮是 `disabled`
//! - 截不到原生窗口、截不到多显示器
//!
//! 两套实现 ⇒ 同一个「截图」在家族里是两种行为。本 crate 把「采像素」这件事
//! 收归能力层，各 app 只负责选框交互与落盘。
//!
//! ## 边界
//!
//! - 本 crate 只做**采集**：枚举显示器/窗口、截取像素。
//! - **编码不在这里** —— JPEG/base64 编码走 cap-img（`encode_frame_jpeg`），
//!   本 crate 通过 `cap-img` 这个依赖调用它，避免同一套编码有两个实现。
//! - **区域选框的交互不在这里** —— 选框是 UI 行为，由各 app 的前端做；
//!   本 crate 只提供「按显示器 + 矩形」精确截取的能力。
//!
//! ## 坐标系（易错点）
//!
//! 区域坐标是**相对于目标显示器左上角**的，不是屏幕全局坐标。
//! 多显示器时每块屏幕有自己的原点（`Monitor::x()` / `Monitor::y()`）。
//! 选框交互在前端做，提交给本 crate 的是**显示器的局部坐标**。

use serde::{Deserialize, Serialize};
use std::path::Path;
use xcap::image::RgbaImage;
use xcap::{Monitor, Window};

/// 一块显示器的信息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DisplayInfo {
    /// 显示器 ID（截屏时用它定位）
    pub id: u32,
    /// 显示器名
    pub name: String,
    /// 逻辑宽度（像素）
    pub width: u32,
    /// 逻辑高度（像素）
    pub height: u32,
    /// 是否主显示器
    pub is_primary: bool,
    /// 在虚拟桌面里的 X 原点（多屏时用）
    pub x: i32,
    /// 在虚拟桌面里的 Y 原点（多屏时用）
    pub y: i32,
}

/// 一个窗口的信息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowInfo {
    /// 窗口 ID
    pub id: u32,
    /// 所属进程 ID
    pub pid: u32,
    /// 应用名（如 `Safari`）
    pub app_name: String,
    /// 窗口标题
    pub title: String,
    /// 窗口左上角 X
    pub x: i32,
    /// 窗口左上角 Y
    pub y: i32,
    /// 窗口宽
    pub width: u32,
    /// 窗口高
    pub height: u32,
    /// 是否最小化（最小化的窗口截出来是空的）
    pub is_minimized: bool,
    /// 是否当前聚焦
    pub is_focused: bool,
}

/// 采集到的一帧像素。
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    /// RGBA 原始像素（4 通道，长度 = width * height * 4）
    pub rgba: Vec<u8>,
    /// 宽
    pub width: u32,
    /// 高
    pub height: u32,
    /// 来源显示器 ID
    pub display_id: u32,
}

impl CapturedFrame {
    /// 把这一帧编码成 JPEG/base64（等比缩放 + RGBA→RGB + JPEG 编码）。
    ///
    /// 编码走 cap-img 的 `encode_frame_jpeg` —— 与 remote 仓同一份实现。
    /// 这条路径的历史 bug 值得记住：曾把 4 通道数据声明成 `Rgb8` 交给编码器，
    /// image 0.25 直接 panic（"Invalid buffer length: expected N*3 got N*4"），
    /// 截屏功能全程不可用。cap-img 侧的长度守卫把这类问题挡在编译期之外。
    pub fn to_jpeg(&self, quality: u8, max_width: u32) -> Result<cap_img::FrameJpeg, String> {
        cap_img::encode_frame_jpeg(&self.rgba, self.width, self.height, quality, max_width)
    }

    /// 把这一帧存成 PNG 文件（走 cap-img 的编码能力）。
    pub fn save_png(&self, dest: impl AsRef<Path>) -> Result<(), String> {
        let dest = dest.as_ref();
        let expected = (self.width as usize)
            .checked_mul(self.height as usize)
            .and_then(|px| px.checked_mul(4))
            .ok_or_else(|| format!("尺寸溢出: {}x{}", self.width, self.height))?;
        if self.rgba.len() != expected {
            return Err(format!(
                "RGBA 数据长度不符: 期望 {} 字节, 实得 {} 字节 ({}x{})",
                expected,
                self.rgba.len(),
                self.width,
                self.height
            ));
        }
        let img = image::RgbaImage::from_raw(self.width, self.height, self.rgba.clone())
            .ok_or_else(|| format!("无法构造 RGBA 图像: {}x{}", self.width, self.height))?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {}", e))?;
        }
        img.save_with_format(dest, image::ImageFormat::Png)
            .map_err(|e| format!("保存图片失败: {}", e))
    }
}

/// 枚举所有显示器。
pub fn list_displays() -> Result<Vec<DisplayInfo>, String> {
    let monitors = Monitor::all().map_err(|e| format!("枚举显示器失败: {}", e))?;
    Ok(monitors.iter().map(display_of).collect())
}

/// 枚举所有窗口。
///
/// 最小化的窗口会被跳过 —— 它们截出来是空的，列出来只会让用户白点一次。
pub fn list_windows() -> Result<Vec<WindowInfo>, String> {
    let windows = Window::all().map_err(|e| format!("枚举窗口失败: {}", e))?;
    Ok(windows
        .iter()
        .filter(|w| !w.is_minimized().unwrap_or(false))
        .map(window_of)
        .collect())
}

/// 显示器信息（单个）。
pub fn display_info(display_id: u32) -> Result<DisplayInfo, String> {
    let monitors = Monitor::all().map_err(|e| format!("枚举显示器失败: {}", e))?;
    let m = monitors
        .iter()
        .find(|m| m.id().unwrap_or(0) == display_id)
        .ok_or_else(|| format!("未找到显示器 ID: {}", display_id))?;
    Ok(display_of(m))
}

/// 取主显示器（没有主显示器时取第一块）。
pub fn primary_display() -> Result<DisplayInfo, String> {
    let monitors = Monitor::all().map_err(|e| format!("枚举显示器失败: {}", e))?;
    let m = monitors
        .iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .or_else(|| monitors.first())
        .ok_or_else(|| "未找到可用显示器".to_string())?;
    Ok(display_of(m))
}

/// 截取整块显示器。
pub fn capture_display(display_id: u32) -> Result<CapturedFrame, String> {
    let monitors = Monitor::all().map_err(|e| format!("枚举显示器失败: {}", e))?;
    let m = monitors
        .iter()
        .find(|m| m.id().unwrap_or(0) == display_id)
        .ok_or_else(|| format!("未找到显示器 ID: {}", display_id))?;

    let image: RgbaImage = m.capture_image().map_err(|e| format!("截屏失败: {}", e))?;
    Ok(frame_of(image, m.id().unwrap_or(0)))
}

/// 截取主显示器。
pub fn capture_primary() -> Result<CapturedFrame, String> {
    let monitors = Monitor::all().map_err(|e| format!("枚举显示器失败: {}", e))?;
    let m = monitors
        .iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .or_else(|| monitors.first())
        .ok_or_else(|| "未找到可用显示器".to_string())?;

    let image: RgbaImage = m.capture_image().map_err(|e| format!("截屏失败: {}", e))?;
    Ok(frame_of(image, m.id().unwrap_or(0)))
}

/// 截取显示器的**一个区域**。
///
/// 坐标是**相对于该显示器左上角**的（不是屏幕全局坐标）。
/// 区域越界或宽高为 0 ⇒ 报错，不做静默裁剪。
pub fn capture_region(
    display_id: u32,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> Result<CapturedFrame, String> {
    if width == 0 || height == 0 {
        return Err(format!("截取区域宽高必须为正: {}x{}", width, height));
    }
    let monitors = Monitor::all().map_err(|e| format!("枚举显示器失败: {}", e))?;
    let m = monitors
        .iter()
        .find(|m| m.id().unwrap_or(0) == display_id)
        .ok_or_else(|| format!("未找到显示器 ID: {}", display_id))?;

    let mw = m.width().unwrap_or(0);
    let mh = m.height().unwrap_or(0);
    // 用 checked_add 防溢出（x + width 在 debug 下溢出即 panic）
    let x_end = x
        .checked_add(width)
        .ok_or_else(|| format!("截取区域尺寸溢出: ({},{})+{}x{}", x, y, width, height))?;
    let y_end = y
        .checked_add(height)
        .ok_or_else(|| format!("截取区域尺寸溢出: ({},{})+{}x{}", x, y, width, height))?;
    if x_end > mw || y_end > mh {
        return Err(format!(
            "截取区域超出显示器范围: 显示器{}x{}, 区域({},{})+{}x{}",
            mw, mh, x, y, width, height
        ));
    }

    let image: RgbaImage = m
        .capture_region(x, y, width, height)
        .map_err(|e| format!("截取区域失败: {}", e))?;
    Ok(frame_of(image, display_id))
}

/// 截取一个窗口。
pub fn capture_window(window_id: u32) -> Result<CapturedFrame, String> {
    let windows = Window::all().map_err(|e| format!("枚举窗口失败: {}", e))?;
    let w = windows
        .iter()
        .find(|w| w.id().unwrap_or(0) == window_id)
        .ok_or_else(|| format!("未找到窗口 ID: {}", window_id))?;

    let image: RgbaImage = w
        .capture_image()
        .map_err(|e| format!("截取窗口失败: {}", e))?;
    let (width, height) = (w.width().unwrap_or(0), w.height().unwrap_or(0));
    Ok(CapturedFrame {
        rgba: image.into_raw(),
        width,
        height,
        display_id: 0,
    })
}

/// 显示器 → DisplayInfo。
fn display_of(m: &Monitor) -> DisplayInfo {
    DisplayInfo {
        id: m.id().unwrap_or(0),
        name: m.name().unwrap_or_default(),
        width: m.width().unwrap_or(0),
        height: m.height().unwrap_or(0),
        is_primary: m.is_primary().unwrap_or(false),
        x: m.x().unwrap_or(0),
        y: m.y().unwrap_or(0),
    }
}

/// 窗口 → WindowInfo。
fn window_of(w: &Window) -> WindowInfo {
    WindowInfo {
        id: w.id().unwrap_or(0),
        pid: w.pid().unwrap_or(0),
        app_name: w.app_name().unwrap_or_default(),
        title: w.title().unwrap_or_default(),
        x: w.x().unwrap_or(0),
        y: w.y().unwrap_or(0),
        width: w.width().unwrap_or(0),
        height: w.height().unwrap_or(0),
        is_minimized: w.is_minimized().unwrap_or(false),
        is_focused: w.is_focused().unwrap_or(false),
    }
}

/// RGBA 图 → CapturedFrame。
fn frame_of(image: RgbaImage, display_id: u32) -> CapturedFrame {
    let (width, height) = image.dimensions();
    CapturedFrame {
        rgba: image.into_raw(),
        width,
        height,
        display_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一帧已知尺寸的 RGBA（与 xcap 真实采集到的形状一致：4 通道）。
    fn frame(w: u32, h: u32) -> CapturedFrame {
        CapturedFrame {
            rgba: vec![128u8; (w * h * 4) as usize],
            width: w,
            height: h,
            display_id: 7,
        }
    }

    /// 回归锚点：ecb895e（remote 仓）把 4 通道数据声明成 Rgb8，
    /// image 0.25 直接 panic，截屏自 2026-09-06 起全程不可用。
    /// cap-img 的长度守卫必须让这条路径拿到可解码的 JPEG。
    #[test]
    fn frame_encodes_to_jpeg_without_panic() {
        let f = frame(64, 48);
        let jpeg = f.to_jpeg(80, 0).expect("合法帧必须编码成功");
        assert_eq!((jpeg.width, jpeg.height), (64, 48));

        use base64::Engine;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&jpeg.base64)
            .expect("base64 必须合法");
        assert_eq!(&raw[..2], &[0xFF, 0xD8], "应是 JPEG 魔数");
    }

    /// 等比缩小语义由 cap-img 承担：0=原样、超宽才缩、高度向上取整。
    #[test]
    fn frame_respects_max_width() {
        let f = frame(100, 33);
        let jpeg = f.to_jpeg(80, 50).unwrap();
        assert_eq!((jpeg.width, jpeg.height), (50, 17));

        // 原图已比 max_width 窄 → 不放大
        let jpeg = f.to_jpeg(80, 500).unwrap();
        assert_eq!((jpeg.width, jpeg.height), (100, 33));
    }

    /// 长度不符必须返回 Err 而非 panic（cap-img 侧的守卫在本 crate 同样可达）。
    #[test]
    fn frame_rejects_mismatched_buffer() {
        let bad = CapturedFrame {
            rgba: vec![128u8; 64 * 48 * 3],
            width: 64,
            height: 48,
            display_id: 0,
        };
        let err = bad.to_jpeg(80, 0).expect_err("长度不符应报错");
        assert!(err.contains("长度不符"), "实得 {}", err);
    }
}
