//! Minimal HTTP/1.1 data-plane server for the Lua Router (TODO **T5.2**).
//!
//! Accepts on a real `TcpListener` and serves each connection as a
//! `tokio::task::spawn_local` task driving the Lua pipeline — openresty's
//! per-connection coroutine model, on the single-thread `LocalSet`. This is the
//! "real client observes wire bytes" surface the T5.2 acceptance requires.
//!
//! HTTP/1.1 parsing is in [`crate::conn`] (shared with the reverse proxy in
//! [`crate::proxy`]). The reverse-proxy data plane ([`crate::proxy::serve_proxy`])
//! is the T5.4 entry point; this module is the Lua-content-only transport.

use std::future::Future;
use std::net::SocketAddr;

use tokio::net::{TcpListener, TcpStream};

use crate::conn::{self, BodyError};
use crate::pipeline::{build_response, Pipeline};

/// Run the Router Lua data plane until `shutdown` resolves.
///
/// Must be driven on a [`tokio::task::LocalSet`] (the caller's): per-connection
/// tasks are `spawn_local`-ed so the `!Send` VM never crosses a thread.
pub async fn serve<F>(pipeline: Pipeline, listener: TcpListener, shutdown: F) -> std::io::Result<()>
where
    F: Future<Output = ()>,
{
    // Boot once: run init_worker_by_lua (if registered) before accepting.
    pipeline.boot().await;

    let addr = listener.local_addr()?;
    tracing::info!(target: "init-pro", %addr, "router data plane listening (HTTP)");
    let pipeline = std::rc::Rc::new(pipeline);
    let mut shutdown = Box::pin(shutdown);

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break,
            res = listener.accept() => {
                let (stream, peer) = match res {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(target: "init-pro", error = %e, "accept failed");
                        continue;
                    }
                };
                let pipeline = pipeline.clone();
                tokio::task::spawn_local(async move {
                    if let Err(e) = handle_connection(stream, &pipeline).await {
                        tracing::warn!(target: "init-pro", %peer, error = %e, "connection closed");
                    }
                });
            }
        }
    }
    tracing::info!(target: "init-pro", "router data plane drained");
    Ok(())
}

async fn handle_connection(mut stream: TcpStream, pipeline: &Pipeline) -> std::io::Result<()> {
    let head = conn::read_head(&mut stream).await?;
    let (req, body_spec) = match conn::parse_request(&head) {
        Ok(v) => v,
        Err(msg) => {
            conn::write_response(&mut stream, conn::error_response(400, &msg)).await?;
            return Ok(());
        }
    };

    let body = match conn::read_body(&mut stream, body_spec).await {
        Ok(b) => b,
        Err(BodyError::TooLarge) => {
            conn::write_response(
                &mut stream,
                conn::error_response(413, "request body too large"),
            )
            .await?;
            return Ok(());
        }
        Err(BodyError::Io(e)) => return Err(e),
    };

    let (parts, _) = req.into_parts();
    let outcome = pipeline.serve_request_with_body(&parts, body).await;
    let response = build_response(outcome);
    conn::write_response(&mut stream, response).await
}

/// Bind an ephemeral loopback listener (mainly for tests): returns addr + listener.
pub fn ephemeral_listener() -> std::io::Result<(SocketAddr, TcpListener)> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    listener.set_nonblocking(true)?;
    let listener = TcpListener::from_std(listener)?;
    Ok((addr, listener))
}
