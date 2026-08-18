use std::sync::Arc;

use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use crate::router::{self, RouterState};

pub async fn serve(bind: &str, state: Arc<RouterState>) -> std::io::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    tracing::info!(%bind, "listening");

    loop {
        let (stream, _peer) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let state = state.clone();

        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |req| {
                router::handle(req, state.clone())
            });
            if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                tracing::warn!(?err, "connection error");
            }
        });
    }
}
