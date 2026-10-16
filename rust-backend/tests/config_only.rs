use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn browser_serves_settings_without_a_monitor_or_boot_preview() {
    let root = std::env::temp_dir().join(format!(
        "sysx-config-only-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("index.html"),
        include_str!("../../rust-ui/index.html"),
    )
    .unwrap();
    let reserve = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let requested = reserve.local_addr().unwrap().port();
    drop(reserve);
    let (port, server) = sysx_backend::web_config::start(
        &root,
        &root,
        "test-local-only-token-never-shipped".into(),
        1,
        requested,
    )
    .await
    .unwrap();
    async fn get(port: u16, path: &str) -> String {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        stream
            .write_all(
                format!(
                    "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }
    let settings = get(port, "/config.html").await;
    assert!(settings.starts_with("HTTP/1.1 200"));
    assert!(settings.contains("Loading SysX settings"));
    assert!(!settings.contains("NO FRAME"));
    assert!(!settings.contains("<canvas"));
    let monitor = get(port, "/monitor.html").await;
    assert!(monitor.starts_with("HTTP/1.1 404"));
    server.abort();
    let _ = server.await;
    std::fs::remove_dir_all(root).unwrap();
}
