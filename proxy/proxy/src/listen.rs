use crate::inbound::Inbound;
use common::tracing;
use proxy_common::{
    anyhow,
    hyper::{server::conn::http1, service::service_fn},
    hyper_util::rt::TokioIo,
    tokio::{
        self,
        net::{TcpListener, TcpStream},
    },
};
use std::net::SocketAddr;

pub struct Listen {
    pub host: String,
}

impl Listen {
    pub fn new() -> Self {
        let config = config::get_config();
        Listen {
            host: config.proxy.addr.clone(),
        }
    }
}

impl Default for Listen {
    fn default() -> Self {
        Listen::new()
    }
}

impl Listen {
    pub async fn listen(&self) -> Result<(), anyhow::Error> {
        let server_addr: SocketAddr = self.host.parse()?;
        // Create a TCP listener which will listen for incoming connections.
        let listener = TcpListener::bind(server_addr).await?;
        tracing::info!("🚀 Proxy server listening on: {}", server_addr);

        // init pool
        proxy_pool::init();

        // Accept incoming TCP connections
        self.accept(listener).await
    }

    async fn accept(&self, listener: TcpListener) -> Result<(), anyhow::Error> {
        loop {
            let (stream, addr) = listener.accept().await?;
            self.handler(TokioIo::new(stream), addr)?;
        }
    }

    fn handler(&self, io: TokioIo<TcpStream>, addr: SocketAddr) -> Result<(), anyhow::Error> {
        tokio::task::spawn(async move {
            let _ = http1::Builder::new()
                .preserve_header_case(true)
                .title_case_headers(true)
                .serve_connection(io, service_fn(|req| Inbound::inbound(req, addr)))
                .with_upgrades()
                .await
                .is_err();
        });
        Ok(())
    }
}
