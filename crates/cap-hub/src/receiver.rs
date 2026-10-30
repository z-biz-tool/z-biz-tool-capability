//! 接收侧胶水 —— 让一个 app 接上 `zbiz://` 深链所需的全部 Rust 侧逻辑。
//!
//! # 为什么这一层存在
//!
//! 13 个仓的 `single-instance` 回调目前全是 `_argv`（丢弃），
//! 意味着「box 发过去的深链，目标 app 收到了但没处理」。
//!
//! 每个仓各写一遍接收逻辑，会得到 13 份**迟早会不一致**的代码 ——
//! 这正是家族要消除的东西。所以接收侧也收口在这里，
//! 各仓只写胶水（把 `AppHandle` 递进来），不写协议。
//!
//! # 用法（每个仓改 4 处）
//!
//! ```ignore
//! // 1) Cargo.toml
//! //    cap-hub = { path = "../../z-biz-tool-capability/crates/cap-hub", features = ["tauri"] }
//!
//! // 2) lib.rs：注册 State + 处理 single-instance 回调
//! let pending = cap_hub::receiver::SharedPending::default();
//! let mut builder = tauri::Builder::default()
//!     .manage(pending.clone())
//!     .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
//!         cap_hub::receiver::handle_second_instance(app, &argv);
//!     }));
//!
//! // 3) lib.rs：冷启动 seed（前端还没挂监听时，深链会丢）
//! cap_hub::receiver::seed_cold_start(&app.handle(), &pending);
//!
//! // 4) commands.rs：暴露取队列的命令（本仓自己写 wrapper，不要直接注册 crate 的）
//! #[tauri::command]
//! pub fn zbiz_take_pending(
//!     pending: State<'_, cap_hub::receiver::SharedPending>,
//! ) -> Vec<cap_hub::Handoff> {
//!     cap_hub::receiver::take_pending_from(pending.inner())
//! }
//! ```
//!
//! 前端侧监听 `zbiz-open`（`cap_hub::EVENT`），
//! 收到后用 `zbiz_take_pending` 命令把队列取空。

use crate::Handoff;
use crate::Pending;
use std::collections::BTreeMap;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager};

/// 队列的共享句柄。放进 Tauri 的 `State` 里各处都能拿到同一份。
///
/// 用 `Arc` 而不是裸 `Pending`：胶水层要在 `setup` 闭包里拿它去 seed，
/// 同时又要把它 `manage` 出去给命令用，两者必须指向同一份队列。
pub type SharedPending = Arc<Pending>;

/// 把一批深链挂进队列，并通知前端。
///
/// 前端监听的事件名是 `zbiz-open`（`cap_hub::EVENT`）。
///
/// 整批一次入队、一次通知：逐条通知会让前端被唤起 N 次，
/// 而这批深链本来就是同一次唤起带来的。
pub fn push_all_and_notify(app: &AppHandle, pending: &Pending, items: Vec<Handoff>) -> usize {
    let added = pending.push(items);
    if added == 0 {
        // 全是重复的深链 —— 不重复通知，避免前端把同一条处理两遍
        return 0;
    }
    let _ = app.emit(crate::EVENT, ());
    added
}

/// 处理「第二个实例被唤起」的情形。
///
/// `argv` 里混着 exe 路径、macOS 的 `-psn_0_12345`、Windows 的 exe 路径，
/// 所以先从中挑出深链，其余忽略。
pub fn handle_second_instance(app: &AppHandle, argv: &[String]) {
    let Some(pending) = app.try_state::<SharedPending>() else {
        // State 没注册 —— 说明这个仓没按用法接。深链丢弃，但把线索打进日志，
        // 让「点了没反应」这类问题有据可查。
        eprintln!("[cap-hub] 收到深链但本应用未注册 pending 状态，深链被丢弃");
        return;
    };

    let items = crate::from_args(argv.iter().cloned());
    if items.is_empty() {
        return;
    }
    for h in &items {
        eprintln!(
            "[cap-hub] 收到深链: zbiz://{}/{} params={:?}",
            h.product, h.action, h.params
        );
    }
    push_all_and_notify(app, pending.inner(), items);
}

/// 冷启动 seed：应用刚起来时 argv 里就带着深链。
///
/// 前端此时还没挂监听，那次信号会丢 —— 所以队列先存着，
/// 前端 mount 后主动 `zbiz_take_pending` 取一次。
pub fn seed_cold_start(_app: &AppHandle, pending: &Pending) {
    let args: Vec<String> = std::env::args().collect();
    let items = crate::from_args(args);
    if items.is_empty() {
        return;
    }
    let added = pending.push(items);
    if added > 0 {
        eprintln!("[cap-hub] 冷启动收到 {added} 条深链");
    }
}

/// 构造一条交给目标 app 的深链。
///
/// `from` 由发送方强制填写，不接受调用方伪造 —— 否则目标应用无法区分
/// 是 box 召唤还是别的产品转手过来的，回传结果时就会找错人。
pub fn build_for(
    target: &str,
    action: &str,
    params: &BTreeMap<String, String>,
    from: &str,
) -> Result<String, String> {
    let mut p = params.clone();
    p.insert("from".to_string(), from.to_string());
    crate::build(target, action, &p)
}

/// 取走队列里的全部深链（前端调用）。
///
/// **纯函数，不带 `#[tauri::command]`** —— 命令名由各仓在自己的
/// `commands.rs` 里定（家族统一为 `zbiz_take_pending`），
/// 这样各仓的「前端 invoke ⇄ Rust 命令」契约测试扫得到它，
/// 而不必去解析本 crate 的源码。
///
/// 各仓的 wrapper 就是一行转发：
///
/// ```ignore
/// #[tauri::command]
/// pub fn zbiz_take_pending(
///     pending: State<'_, cap_hub::receiver::SharedPending>,
/// ) -> Vec<cap_hub::Handoff> {
///     cap_hub::receiver::take_pending_from(pending.inner())
/// }
/// ```
pub fn take_pending_from(pending: &Pending) -> Vec<Handoff> {
    pending.take()
}
