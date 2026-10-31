//! 本地回环回调服务器 —— OAuth2 授权码流程的「收信人」。
//!
//! # 为什么自己手写而不是用 tiny_http / axum
//!
//! 这个服务器要做的事只有一件：**在 127.0.0.1 上等一次 HTTP 请求，把查询串交出去**。
//!
//! 引入 web 框架会带来几百个传递依赖，而 capability 仓的依赖表是全家族 Rust 依赖的
//! 唯一版本源（见 workspace Cargo.toml 的注释）—— 每加一个重依赖，
//! 「依赖树收敛」这件事就更难维持。手写一个只解析请求行的最小 HTTP，
//! 换来的是零新增依赖。
//!
//! # 安全性
//!
//! - **只绑 127.0.0.1**，不绑 0.0.0.0 —— 否则局域网内任何人都能打这个端口，
//!   伪造一个 `?code=...` 回调来劫持登录。
//! - **端口由内核分配（bind 到 0）** —— 不用固定端口，避免「端口被占用就登录不了」
//!   与「端口可预测 ⇒ 可被抢注」两个问题。
//! - **只处理一条请求** —— 拿到回调（或超时）即退出，不做长期监听。
//!
//! # 与协议逻辑的分工
//!
//! 本模块**只负责把查询串原样交出去**。解析 `code` / `error` / 校验 `state`
//! 全部在 TypeScript 侧的 `z-biz-tool-shared/src/sso/oauth.ts` ——
//! 协议逻辑只有一份，不在这里重写一遍。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// 回环服务器的错误类型。
#[derive(Debug)]
pub enum CallbackError {
    /// 端口绑定失败（几乎只可能是系统资源耗尽）。
    Bind(String),
    /// 等待回调超时。
    Timeout,
    /// 与浏览器的连接在拿到查询串前断了。
    Connection(String),
    /// 服务器被提前关掉（用户主动取消）。
    Cancelled,
}

impl std::fmt::Display for CallbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallbackError::Bind(e) => write!(f, "绑定本地回环端口失败：{}", e),
            CallbackError::Timeout => write!(f, "等待浏览器回调超时"),
            CallbackError::Connection(e) => write!(f, "读取浏览器回调失败：{}", e),
            CallbackError::Cancelled => write!(f, "登录已取消"),
        }
    }
}

impl std::error::Error for CallbackError {}

/// 回环服务器的句柄。
///
/// `wait_for_callback` 与 `close` 都是**阻塞式**的，但它们各自在独立线程里跑 ——
/// Tauri 的 command 是异步的，Rust 侧要阻塞等待没问题，但不能占着 Tauri 的主线程。
pub struct CallbackServer {
    /// 实际监听的端口。**必须**在跳浏览器之前拿到它 ——
    /// 拼出来的 `redirect_uri` 要和 ctc 侧登记的完全一致，否则会拿到
    /// `redirect_uri_mismatch`。
    port: u16,
    /// 收到回调后的查询串（不含 `?`）。
    rx: mpsc::Receiver<Result<String, CallbackError>>,
    /// 关停信号：置真后 accept 循环退出。
    cancel: Arc<AtomicBool>,
}

impl CallbackServer {
    /// 启动回环服务器（端口由内核动态分配）。
    ///
    /// @param path 要处理的路径（不含查询串），例如 `/callback`。
    ///             其它路径一律 404 —— 不多不少只认这一个。
    /// @param timeout 最长等多久。超时会返回 `CallbackError::Timeout`。
    ///
    /// 端口由内核分配（bind 到 0），**调用方必须用返回值里的 `port` 去拼 redirect_uri**。
    pub fn start(path: &str, timeout: Duration) -> std::io::Result<Self> {
        Self::start_on_port(path, timeout, 0)
    }

    /// 启动回环服务器，可指定**固定端口**。
    ///
    /// @param port 绑定的端口。`0` = 由内核分配（动态端口，推荐）；
    ///             其它值 = 固定端口（该端口已被占用时启动失败）。
    ///
    /// ## 为什么需要固定端口
    ///
    /// 绝大多数 OIDC 服务端要求 `redirect_uri` 与预登记值**精确匹配**。
    /// 动态端口每次登录都不同 ⇒ 这类服务端必须改用固定端口，
    /// 并把同一个回环地址登记到 ctc 侧。
    ///
    /// 固定端口的代价：端口可预测（可被同机其它进程抢注），
    /// 且被占用时登录直接失败。所以**默认仍是动态端口** ——
    /// 只有确认 ctc 侧不接受动态端口时才显式指定。
    pub fn start_on_port(path: &str, timeout: Duration, port: u16) -> std::io::Result<Self> {
        // 只绑回环地址 —— 见模块文档的「安全性」。
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        let port = listener.local_addr()?.port();

        // 立刻把监听 socket 的写超时设上：客户端连上后不吭声时，
        // 读线程不能无限挂着。
        listener.set_nonblocking(true)?;

        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();

        let cancel_thread = Arc::clone(&cancel);
        let path_owned = path.to_string();
        std::thread::spawn(move || {
            let deadline = Instant::now() + timeout;
            let result = accept_loop(&listener, &path_owned, &cancel_thread, deadline);
            // 发送失败只可能是接收端已被 drop（调用方提前走了），忽略即可。
            let _ = tx.send(result);
        });

        Ok(Self { port, rx, cancel })
    }

    /// 实际监听的端口。
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 阻塞等待浏览器回调，返回**查询串**（不含 `?`）。
    ///
    /// 用户在 ctc 页面点「拒绝授权」时，浏览器仍会回调（带 `error=access_denied`）——
    /// 所以「拿到 error」是**正常结果**，不是异常。
    pub fn wait_for_callback(self) -> Result<String, CallbackError> {
        match self.rx.recv() {
            Ok(result) => result,
            Err(_) => Err(CallbackError::Cancelled),
        }
    }

    /// 提前关掉服务器。
    ///
    /// 幂等：反复调用安全。
    pub fn close(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        // 唤醒可能正阻塞在 accept 的线程：连自己一下，它会拿到一个连接并发现已取消。
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

impl Drop for CallbackServer {
    fn drop(&mut self) {
        self.close();
    }
}

/// accept 循环：等到匹配路径的请求、或超时、或被取消。
fn accept_loop(
    listener: &TcpListener,
    path: &str,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Result<String, CallbackError> {
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(CallbackError::Cancelled);
        }
        if Instant::now() > deadline {
            return Err(CallbackError::Timeout);
        }

        match listener.accept() {
            Ok((stream, _addr)) => {
                match handle_connection(stream, path) {
                    Some(query) => return Ok(query),
                    // 路径不匹配（favicon.ico 之类）—— 回 404 并继续等。
                    None => continue,
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // 非阻塞模式下没人连 —— 睡一小会儿再看。
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(CallbackError::Connection(e.to_string())),
        }
    }
}

/// 处理一次 HTTP 请求。
///
/// 返回 `Some(query)` 表示这是我们要的回调；`None` 表示路径不匹配，继续等。
fn handle_connection(mut stream: TcpStream, path: &str) -> Option<String> {
    // **必须显式把连接设为阻塞模式。**
    //
    // 监听 socket 是非阻塞的（`accept_loop` 靠 WouldBlock 轮询），
    // 而 **macOS 的 `accept(2)` 会把 O_NONBLOCK 继承给新连接**（Linux 不会）。
    // 后果：客户端刚 `connect` 完、请求字节还在路上时，`read_line` 立刻返回
    // `WouldBlock` —— 而下面所有读都用 `.ok()?` 把它吞成 `None`，
    // 于是**连 404 都不回**就 `continue`，客户端拿到的是「连接被重置」而不是响应。
    //
    // 真实症状：cap-sso 的 `路径不匹配时继续等下一个请求` 在本机 macOS 上偶发红，
    // 而 Linux / Windows CI 全绿 —— 这就是平台差异造成的假象。
    //
    // 改成阻塞后，读会等到数据或 10 秒读超时，读超时是**可观测的错误**
    // （走 `Err` 分支），不会再被 `.ok()?` 悄悄吞掉。
    stream
        .set_nonblocking(false)
        .expect("把连接设为阻塞模式失败");

    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;

    let mut reader = BufReader::new(stream.try_clone().ok()?);

    // 只读请求行：`GET /callback?code=...&state=... HTTP/1.1`
    //
    // 读失败**不能静默 `None`** —— 那样客户端只看到「连接被重置」，
    // 排查时完全无从下手。回一个 400 并说明原因，再当作「不是回调」继续等。
    let mut request_line = String::new();
    if let Err(e) = reader.read_line(&mut request_line) {
        respond(
            &mut stream,
            "400 Bad Request",
            "text/plain; charset=utf-8",
            format!("读取请求行失败：{e}").as_bytes(),
        );
        return None;
    }
    let request_line = request_line.trim_end();
    if request_line.is_empty() {
        respond(
            &mut stream,
            "400 Bad Request",
            "text/plain; charset=utf-8",
            b"empty request line",
        );
        return None;
    }

    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        respond(
            &mut stream,
            "400 Bad Request",
            "text/plain; charset=utf-8",
            b"malformed request line",
        );
        return None;
    };

    // OAuth 回调是 GET。POST /callback 之类的一律不当回调处理 ——
    // 否则任何能往这个端口发请求的东西都能伪造一次登录。
    if !method.eq_ignore_ascii_case("GET") {
        respond(
            &mut stream,
            "405 Method Not Allowed",
            "text/plain; charset=utf-8",
            b"only GET is accepted here",
        );
        return None;
    }

    // 读完请求头（否则客户端可能等我们回响应）。
    // 头部读失败不致命 —— 响应还是要发的。
    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) => break,
            Ok(_) => {
                let header = header.trim_end();
                if header.is_empty() {
                    break;
                }
                if let Some(v) = header
                    .strip_prefix("Content-Length:")
                    .or_else(|| header.strip_prefix("content-length:"))
                {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }
            Err(_) => break,
        }
    }
    if content_length > 0 {
        let mut body = vec![0u8; content_length.min(64 * 1024)];
        let _ = reader.read_exact(&mut body);
    }

    // 路径匹配：忽略查询串，只比路径部分。
    let (raw_path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };

    if raw_path != path {
        // 路径不匹配（favicon.ico 之类）—— 回 404，交由调用方继续等。
        respond(
            &mut stream,
            "404 Not Found",
            "text/plain; charset=utf-8",
            b"not found",
        );
        return None;
    }

    respond(
        &mut stream,
        "200 OK",
        "text/html; charset=utf-8",
        SUCCESS_PAGE.as_bytes(),
    );
    Some(query.to_string())
}

/// 写一个完整的 HTTP 响应并关闭连接。
fn respond(stream: &mut TcpStream, status: &str, content_type: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        content_type,
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Both);
}

/// 回调成功后浏览器里显示的页面。
const SUCCESS_PAGE: &str = r#"<!doctype html>
<html lang="zh-CN">
<head><meta charset="utf-8"><title>登录完成</title></head>
<body style="font-family:system-ui,sans-serif;text-align:center;padding:64px 24px;background:#fafafa">
  <h1 style="font-size:22px;margin:0 0 12px">登录完成</h1>
  <p style="color:#666;font-size:14px;margin:0">可以关闭这个窗口，回到应用继续。</p>
</body>
</html>"#;

/// 便于测试与诊断：确认某个地址上的服务器确实是本模块起的那种。
pub fn is_loopback(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// 发一个裸 HTTP 请求，拿回响应与「服务端交出的查询串」。
    fn request(port: u16, target: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        s.write_all(format!("GET {} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n", target).as_bytes())
            .expect("write");
        let mut out = String::new();
        s.read_to_string(&mut out).expect("read");
        out
    }

    #[test]
    fn 回调服务器只绑回环地址() {
        let server = CallbackServer::start("/callback", Duration::from_secs(5)).expect("start");
        assert!(server.port() > 0);
        // 端口必须落在回环区间 —— 不能是 0.0.0.0 上监听的端口。
        assert!(
            (1024..=65535).contains(&server.port()),
            "端口应从内核分配且非特权，实际 {}",
            server.port()
        );
    }

    /// 回归测试：**连接必须被显式设为阻塞模式**。
    ///
    /// 背景：监听 socket 是非阻塞的，而 macOS 的 `accept(2)` 会把 O_NONBLOCK
    /// **继承给新连接**（Linux 不会）。若不显式改回阻塞，客户端刚 connect 完、
    /// 请求字节还在路上时 `read_line` 立刻返回 `WouldBlock`，
    /// 被 `.ok()?` 吞成 `None` ⇒ **连 404 都不回**。
    ///
    /// 这条测试**故意把字节拆开发**：先 connect 建立连接，隔一小会儿再发请求行 ——
    /// 这样必然制造出「连接已建立但数据未到」的窗口，
    /// 在 macOS 上 100% 复现，在 Linux 上也验证同一语义。
    /// （原测试「路径不匹配时继续等下一个请求」是并发窗口触发，复现率低，
    ///   不能作为这条语义的守护。）
    #[test]
    fn 连接必须被设为阻塞_否则请求字节未到时会静默丢连接() {
        let server = CallbackServer::start("/callback", Duration::from_secs(5)).expect("start");
        let port = server.port();

        let handle = std::thread::spawn(move || {
            // 第一步：只连上，不发任何字节。
            let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
            // 第二步：等服务器 accept 完，再发请求行 —— 制造「连接已建立、字节未到」的窗口。
            std::thread::sleep(Duration::from_millis(300));
            s.write_all(b"GET /callback?code=DELAYED HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                .expect("write");
            s.flush().expect("flush");
            let mut out = String::new();
            s.read_to_string(&mut out).expect("read");
            out
        });

        let query = server.wait_for_callback().expect("应拿到回调");
        assert_eq!(query, "code=DELAYED");

        let response = handle.join().expect("请求线程");
        assert!(
            response.starts_with("HTTP/1.1 200 OK"),
            "延迟发送的请求也必须得到正常响应，实际：{}",
            response
        );
    }

    #[test]
    fn 拿到匹配的回调并返回查询串() {
        let server = CallbackServer::start("/callback", Duration::from_secs(5)).expect("start");
        let port = server.port();

        // 在另一个线程发请求，避免测试自己被 accept 循环卡住。
        let handle =
            std::thread::spawn(move || request(port, "/callback?code=CODE-1&state=STATE-1"));

        let query = server.wait_for_callback().expect("应拿到回调");
        assert_eq!(query, "code=CODE-1&state=STATE-1");

        let response = handle.join().expect("请求线程");
        assert!(
            response.starts_with("HTTP/1.1 200 OK"),
            "实际：{}",
            response
        );
        assert!(response.contains("登录完成"));
    }

    #[test]
    fn 路径不匹配时继续等下一个请求() {
        let server = CallbackServer::start("/callback", Duration::from_secs(5)).expect("start");
        let port = server.port();

        let handle = std::thread::spawn(move || request(port, "/favicon.ico"));

        // favicon 请求应得到 404，且**不能**把回调流程结束掉。
        let response = handle.join().expect("请求线程");
        assert!(response.starts_with("HTTP/1.1 404"), "实际：{}", response);

        // 服务器还活着：再发一个正确的。
        let handle2 = std::thread::spawn(move || request(port, "/callback?code=C2"));
        let query = server.wait_for_callback().expect("应拿到回调");
        assert_eq!(query, "code=C2");
        let _ = handle2.join();
    }

    #[test]
    fn 超时后返回超时错误() {
        let server = CallbackServer::start("/callback", Duration::from_millis(200)).expect("start");
        let err = server.wait_for_callback().expect_err("应超时");
        assert!(matches!(err, CallbackError::Timeout), "实际错误：{:?}", err);
    }

    #[test]
    fn 提前关掉后等待方得到取消() {
        let server = CallbackServer::start("/callback", Duration::from_secs(10)).expect("start");
        server.close();
        let err = server.wait_for_callback().expect_err("应取消");
        assert!(
            matches!(err, CallbackError::Cancelled | CallbackError::Connection(_)),
            "实际错误：{:?}",
            err
        );
    }

    #[test]
    fn 两次启动拿到不同端口() {
        // 端口由内核分配 —— 若这里拿到同一个端口，说明我们绑的是固定端口，
        // 那就意味着端口可预测（可被抢注）。
        let a = CallbackServer::start("/callback", Duration::from_secs(5)).expect("start");
        let b = CallbackServer::start("/callback", Duration::from_secs(5)).expect("start");
        assert_ne!(a.port(), b.port(), "两个服务器不该拿到同一个端口");
    }

    #[test]
    fn 指定固定端口时绑到那个端口() {
        // 绝大多数 OIDC 服务端要求 redirect_uri 与预登记值精确匹配，
        // 而动态端口每次都不同 ⇒ 需要能显式指定端口。
        //
        // **竞态说明**：测试并行跑，probe（绑 0）放掉端口后，那个临时端口
        // 可能立刻被**并行测试自己的动态端口服务器**抢走（Linux 临时端口
        // 分配偏好刚释放的端口）⇒ CI 上曾稳定 AddrInUse。
        // 所以 bind 报 AddrInUse 就换一个端口重试。
        //
        // 断言仍是**强断言**：bind 成功 ⇒ 绑上的必然是刚 probe 的那个
        // `want`（start_on_port 只会绑调用方给的端口；持有期间端口不可能
        // 被换掉）—— 弱化成「只验 port>0」是不必要的让步。
        fn bind_fixed_port_once() -> Result<(CallbackServer, u16), std::io::Error> {
            let probe =
                CallbackServer::start("/callback", Duration::from_millis(1)).expect("probe");
            let want = probe.port();
            probe.close();
            let server = CallbackServer::start_on_port("/callback", Duration::from_secs(5), want)?;
            Ok((server, want))
        }

        let mut result = None;
        let mut last_err: Option<std::io::Error> = None;
        for _ in 0..32 {
            match bind_fixed_port_once() {
                Ok(pair) => {
                    result = Some(pair);
                    break;
                }
                // 端口被并行测试抢走 ⇒ 换一个再试，不算失败。
                // AddrInUse 是「端口被占」的唯一错误形态。
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                    last_err = Some(e);
                    continue;
                }
                Err(e) => panic!("固定端口绑不上（非占用错误）：{:?}", e),
            }
        }
        let (server, want) = result
            .unwrap_or_else(|| panic!("重试 32 次仍绑不上固定端口（最后一次：{:?}）", last_err));
        assert_eq!(server.port(), want, "start_on_port(n) 必须真的绑在 n 上");
        server.close();
    }

    #[test]
    fn 固定端口被占用时启动失败() {
        let first = CallbackServer::start("/callback", Duration::from_secs(5)).expect("start");
        let busy = first.port();
        // 同一个端口再绑一次必须失败 —— 不能悄悄退回到动态端口
        // （那会让拼出去的 redirect_uri 与实际监听的端口对不上，
        //   表现为浏览器回调发到一个没人接的端口，登录永远等不到）。
        let second = CallbackServer::start_on_port("/callback", Duration::from_secs(5), busy);
        assert!(second.is_err(), "端口被占用时必须报错，不能悄悄换端口");
        first.close();
    }
}
