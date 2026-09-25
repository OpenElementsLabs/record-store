//! The HTTP accept loop both listeners run.
//!
//! `axum::serve` offers no way to bound how long a client may take to send its
//! request, so a client that opened a connection and trickled one header line
//! a second held it -- a descriptor and a task -- for as long as it liked, and
//! nothing limited how many it could open. This loop serves the same router
//! the same way (HTTP/1.1 and HTTP/2 over cleartext, the caller's address as
//! `ConnectInfo`, a graceful drain) with two deadlines:
//!
//! * hyper's header read timeout, which bounds every HTTP/1 request head and
//!   the idle wait for the next request on a kept-alive connection;
//! * a deadline for the connection's first request, which also covers the
//!   bytes hyper reads before it knows which protocol it is speaking, and
//!   HTTP/2, which the header timeout does not reach.

use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::Request;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::time::{sleep, timeout};
use tower::ServiceExt;
use tracing::{debug, error};

use crate::ServerError;

/// Serves until shutdown is requested, then drains requests up to
/// `grace_period`.
///
/// A connection that has not delivered a complete request head within
/// `header_read_timeout` of being accepted -- or, later, within that long of
/// the previous response -- is closed.
pub async fn serve<F>(
    listener: TcpListener,
    application: Router,
    shutdown: F,
    grace_period: Duration,
    header_read_timeout: Duration,
) -> Result<(), ServerError>
where
    F: Future<Output = ()> + Send + 'static,
{
    let mut builder = Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(header_read_timeout);
    builder.http2().timer(TokioTimer::new());
    let graceful = GracefulShutdown::new();
    tokio::pin!(shutdown);
    loop {
        let (stream, remote) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(error) => {
                    accept_failed(&error).await;
                    continue;
                }
            },
            () = &mut shutdown => break,
        };
        let first_request = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&first_request);
        let service = application
            .clone()
            .map_request(move |request: Request<Incoming>| {
                seen.store(true, Ordering::Relaxed);
                let mut request = request.map(Body::new);
                request.extensions_mut().insert(ConnectInfo(remote));
                request
            });
        let connection = builder
            .serve_connection_with_upgrades(TokioIo::new(stream), TowerToHyperService::new(service))
            .into_owned();
        let connection = graceful.watch(connection);
        tokio::spawn(async move {
            tokio::select! {
                result = connection => {
                    if let Err(error) = result {
                        debug!(%error, %remote, "connection ended with an error");
                    }
                }
                () = first_request_overdue(header_read_timeout, first_request) => {
                    debug!(%remote, "closing a connection that sent no request in time");
                }
            }
        });
    }
    // Stop accepting before draining, so the drain has an end.
    drop(listener);
    timeout(grace_period, graceful.shutdown())
        .await
        .map_err(|_| ServerError::ShutdownTimeout(grace_period))
}

/// Resolves once `limit` has passed without the connection's first request,
/// and never if the request arrived in time.
async fn first_request_overdue(limit: Duration, first_request: Arc<AtomicBool>) {
    sleep(limit).await;
    if first_request.load(Ordering::Relaxed) {
        std::future::pending::<()>().await;
    }
}

/// Keeps accepting through errors, as `axum::serve` does. A failure that
/// belongs to one connection is skipped; anything else, such as running out
/// of descriptors, is logged and retried after a pause rather than spun on.
async fn accept_failed(error: &std::io::Error) {
    use std::io::ErrorKind::{ConnectionAborted, ConnectionRefused, ConnectionReset};
    if matches!(
        error.kind(),
        ConnectionAborted | ConnectionRefused | ConnectionReset
    ) {
        return;
    }
    error!(%error, "accepting a connection failed; retrying in one second");
    sleep(Duration::from_secs(1)).await;
}

// Keeps the service's error type honest: the router never fails a request, it
// answers it, so a connection only ever ends on an I/O or protocol error.
const _: fn() = || {
    fn infallible<S: tower::Service<Request<Body>, Error = Infallible>>() {}
    infallible::<Router>();
};

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::time::Instant;

    use axum::routing::get;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio::sync::oneshot;

    use super::*;

    const LIMIT: Duration = Duration::from_millis(800);

    /// Serves a router that answers `/` with the caller's address and `/slow`
    /// after a pause, and returns its address and a shutdown trigger.
    async fn server() -> (
        SocketAddr,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<Result<(), ServerError>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let application =
            Router::new()
                .route(
                    "/",
                    get(|ConnectInfo(remote): ConnectInfo<SocketAddr>| async move {
                        remote.to_string()
                    }),
                )
                .route(
                    "/slow",
                    get(|| async {
                        sleep(Duration::from_millis(1_500)).await;
                        "done"
                    }),
                );
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(serve(
            listener,
            application,
            async move {
                let _ = stopped.await;
            },
            Duration::from_secs(10),
            LIMIT,
        ));
        (address, stop, task)
    }

    /// Reads until the server closes the connection, returning what it sent
    /// and how long that took. Gives up well after the limit, so a server that
    /// never closes fails the test rather than hanging it.
    async fn read_until_closed(stream: &mut TcpStream) -> (Vec<u8>, Duration) {
        let started = Instant::now();
        let mut received = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            match timeout(LIMIT * 6, stream.read(&mut buffer)).await {
                Ok(Ok(0) | Err(_)) => return (received, started.elapsed()),
                Ok(Ok(read)) => received.extend_from_slice(&buffer[..read]),
                Err(_) => panic!(
                    "the server kept the connection open for {:?}",
                    started.elapsed()
                ),
            }
        }
    }

    #[tokio::test]
    async fn a_client_that_trickles_its_headers_is_disconnected() {
        let (address, _stop, _task) = server().await;
        let stream = TcpStream::connect(address).await.expect("connect");
        let (mut reader, mut writer) = stream.into_split();
        writer
            .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n")
            .await
            .expect("write");
        // One header line every 200 ms, for far longer than the limit: every
        // line resets nothing, because the limit is on the whole head.
        let trickle = tokio::spawn(async move {
            for index in 0..40 {
                sleep(Duration::from_millis(200)).await;
                if writer
                    .write_all(format!("X-Slow-{index}: 1\r\n").as_bytes())
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
        let started = Instant::now();
        let mut buffer = [0_u8; 1024];
        loop {
            match timeout(LIMIT * 4, reader.read(&mut buffer)).await {
                Ok(Ok(0) | Err(_)) => break,
                Ok(Ok(_)) => {}
                Err(_) => panic!("a trickling client was held for {:?}", started.elapsed()),
            }
        }
        assert!(
            started.elapsed() < LIMIT * 2,
            "closed after {:?}",
            started.elapsed()
        );
        trickle.abort();
    }

    #[tokio::test]
    async fn connections_that_never_send_a_request_are_closed() {
        let (address, _stop, _task) = server().await;
        // Nothing at all, and the first bytes of the HTTP/2 preface, which
        // leave hyper waiting to decide which protocol this is.
        for opening in [b"".as_slice(), b"PRI * HTTP/2".as_slice()] {
            let mut stream = TcpStream::connect(address).await.expect("connect");
            stream.write_all(opening).await.expect("write");
            let (received, waited) = read_until_closed(&mut stream).await;
            assert!(
                waited >= LIMIT / 2,
                "closed after {waited:?}, before the limit"
            );
            assert!(waited < LIMIT * 3, "held for {waited:?}");
            assert!(
                received.is_empty() || received.starts_with(b"HTTP/1.1 408"),
                "{received:?}"
            );
        }
    }

    #[tokio::test]
    async fn requests_are_served_with_their_address_and_idle_connections_expire() {
        let (address, _stop, _task) = server().await;
        let mut stream = TcpStream::connect(address).await.expect("connect");
        let local = stream.local_addr().expect("local address");
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .expect("write");
        let mut buffer = [0_u8; 1024];
        let read = stream.read(&mut buffer).await.expect("response");
        let response = String::from_utf8_lossy(&buffer[..read]).into_owned();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(
            response.ends_with(&local.to_string()),
            "ConnectInfo is the caller: {response}"
        );
        // Kept alive, then idle: closed once the next request head is overdue.
        let (_, waited) = read_until_closed(&mut stream).await;
        assert!(
            waited < LIMIT * 3,
            "an idle connection was held for {waited:?}"
        );
    }

    #[tokio::test]
    async fn shutdown_drains_a_request_in_flight_and_stops_accepting() {
        let (address, stop, task) = server().await;
        let mut stream = TcpStream::connect(address).await.expect("connect");
        stream
            .write_all(b"GET /slow HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .expect("write");
        sleep(Duration::from_millis(200)).await;
        stop.send(()).expect("stop");
        let (received, _) = read_until_closed(&mut stream).await;
        let response = String::from_utf8_lossy(&received);
        assert!(
            response.starts_with("HTTP/1.1 200") && response.ends_with("done"),
            "{response}"
        );
        task.await.expect("join").expect("a clean drain");
        assert!(
            TcpStream::connect(address).await.is_err(),
            "the listener is closed"
        );
    }
}
