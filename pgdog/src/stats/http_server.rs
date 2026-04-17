use std::convert::Infallible;
use std::net::SocketAddr;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tracing::info;

use super::{Clients, MirrorStatsMetrics, Pools, QueryCache, TwoPc};
use crate::quota;

pub fn quota_metrics() -> String {
    let statuses = quota::all_quota_statuses();
    if statuses.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    out.push_str("# HELP pgdog_db_size_bytes Current database size in bytes\n");
    out.push_str("# TYPE pgdog_db_size_bytes gauge\n");
    for s in &statuses {
        out.push_str(&format!(
            "pgdog_db_size_bytes{{database=\"{}\"}} {}\n",
            s.database, s.current_size
        ));
    }

    out.push_str("# HELP pgdog_db_size_limit_bytes Configured maximum database size in bytes\n");
    out.push_str("# TYPE pgdog_db_size_limit_bytes gauge\n");
    for s in &statuses {
        out.push_str(&format!(
            "pgdog_db_size_limit_bytes{{database=\"{}\"}} {}\n",
            s.database, s.max_size
        ));
    }

    out.push_str(
        "# HELP pgdog_db_over_limit Whether the database exceeds its size quota (1=over, 0=ok)\n",
    );
    out.push_str("# TYPE pgdog_db_over_limit gauge\n");
    for s in &statuses {
        out.push_str(&format!(
            "pgdog_db_over_limit{{database=\"{}\"}} {}\n",
            s.database,
            if s.over_limit { 1 } else { 0 }
        ));
    }

    out
}

async fn metrics(_: Request<hyper::body::Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let clients = Clients::load();
    let pools = Pools::load();
    let mirror_stats: Vec<_> = MirrorStatsMetrics::load()
        .into_iter()
        .map(|m| m.to_string())
        .collect();
    let mirror_stats = mirror_stats.join("\n");
    let query_cache: Vec<_> = QueryCache::load()
        .metrics()
        .into_iter()
        .map(|m| m.to_string())
        .collect();
    let query_cache = query_cache.join("\n");
    let two_pc = TwoPc::load();
    let quota = quota_metrics();
    let metrics_data = clients.to_string()
        + "\n"
        + &pools.to_string()
        + "\n"
        + &mirror_stats
        + "\n"
        + &query_cache
        + "\n"
        + &two_pc.to_string()
        + "\n"
        + &quota;
    let response = Response::builder()
        .header(
            hyper::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )
        .body(Full::new(Bytes::from(metrics_data)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::from("Metrics unavailable"))));

    Ok(response)
}

pub async fn server(port: u16) -> std::io::Result<()> {
    info!("OpenMetrics endpoint http://0.0.0.0:{}", port);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr).await?;

    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);

        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service_fn(metrics))
                .await
            {
                eprintln!("OpenMetrics endpoint error: {:?}", err);
            }
        });
    }
}
