use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::config::Config;
use crate::handler::RequestHandler;

/// Drive a single persistent connection: read framed requests, hand each to the
/// per-connection handler, and write the framed response. Requests on one connection
/// are processed sequentially, matching the C++ SQLiteSocket.
///
/// Framing (both directions): a 4-byte little-endian u32 length followed by that many
/// bytes of UTF-8 JSON.
pub async fn handle(mut stream: TcpStream, config: Arc<Config>) -> std::io::Result<()> {
    // Each connection owns its own database-connection cache.
    let mut handler = RequestHandler::new(Arc::clone(&config));

    loop {
        // Read the 4-byte length header. EOF here means the client closed the connection.
        let mut header = [0u8; 4];
        if stream.read_exact(&mut header).await.is_err() {
            return Ok(());
        }
        let packet_size = u32::from_le_bytes(header);

        if packet_size > config.client_max_packet_size {
            eprintln!("Client max allowed packet size reached: {packet_size}");
            return Ok(()); // close the connection
        }

        let mut buf = vec![0u8; packet_size as usize];
        if stream.read_exact(&mut buf).await.is_err() {
            return Ok(());
        }
        let request = String::from_utf8_lossy(&buf).into_owned();

        // SQLite work is blocking. `block_in_place` lets us run it on the current
        // worker thread without starving the runtime's other tasks.
        let response = tokio::task::block_in_place(|| handler.handle_request(&request));

        let body = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        let mut out = Vec::with_capacity(4 + body.len());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body.as_bytes());
        stream.write_all(&out).await?;
    }
}
