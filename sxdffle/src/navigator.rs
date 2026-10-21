use ruffle_frontend_utils::backends::navigator::NavigatorInterface;
use std::fs::File;
use std::path::Path;
use url::Url;

#[derive(Clone)]
pub struct AndroidNavigatorInterface;

// TODO: Prompt the user for these things!
impl NavigatorInterface for AndroidNavigatorInterface {
    fn navigate_to_website(&self, url: Url) {
        // TODO: Implement `ask` behavior!
        let _ = webbrowser::open(url.as_ref());
    }

    async fn open_file(&self, path: &Path) -> std::io::Result<File> {
        File::open(path)
    }

    /// SXDffle：仅在 `SocketMode::Ask` 时会走到这里；build 时我们固定为
    /// `SocketMode::Allow`，因此 TCP 长连接（页游的核心通信方式）直接放行，
    /// 无需目标服务器在 843 端口提供 crossdomain.xml 策略文件。
    /// 保留本实现作为兜底，并打印日志便于抓《神仙道》等页游的连接目标。
    async fn confirm_socket(&self, host: &str, port: u16) -> bool {
        log::info!("SXDffle socket: allowing {host}:{port}");
        true
    }
}
