//! The `wyrd-mcp` binary: MCP stdio transport — one JSON-RPC message per
//! line on stdin, one per line on stdout. Logging (there is none today) would
//! go to stderr; stdout carries protocol frames only.

use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = wyrd_mcp::handle_line(&line) {
            if serde_json::to_writer(&mut stdout, &response).is_err()
                || stdout.write_all(b"\n").is_err()
                || stdout.flush().is_err()
            {
                break; // client hung up
            }
        }
    }
}
