// Copyright (c) 2026 The Ycash developers
// Distributed under the MIT software license, see the accompanying
// file LICENSE or https://www.opensource.org/licenses/mit-license.php .

//! `GET /status`: a one-document HTTP endpoint so `monitor-quote.sh`-style checks can read
//! the pool (height, template age, miners, last tag kind, last submit verdict).

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::debug;

use crate::state::State;

pub async fn serve(state: State, listener: TcpListener) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else { continue };
        let st = state.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let n = match tokio::time::timeout(std::time::Duration::from_secs(5), socket.read(&mut buf)).await {
                Ok(Ok(n)) => n,
                _ => return,
            };
            let request = String::from_utf8_lossy(&buf[..n]);
            let first = request.lines().next().unwrap_or("");
            let mut parts = first.split_whitespace();
            let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            let (status, body) =
                if method == "GET" && (path == "/status" || path == "/") { ("200 OK", st.status_json().to_string()) } else { ("404 Not Found", "{\"error\":\"not found\"}".to_string()) };
            debug!("status: {} {} -> {}", method, path, status);
            let response = format!("HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", status, body.len(), body);
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
    }
}
