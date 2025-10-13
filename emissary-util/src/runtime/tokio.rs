// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

use emissary_core::runtime::{
    AsyncRead, AsyncWrite, Counter, Gauge, Histogram, Instant as InstantT, JoinSet, MetricType,
    MetricsHandle, Runtime as RuntimeT, TcpListener, TcpStream, UdpSocket,
};
use flate2::{
    write::{GzDecoder, GzEncoder},
    Compression,
};
use futures::{AsyncRead as _, AsyncWrite as _, Stream};
use rand_core::{CryptoRng, RngCore};
use tokio::{net, task, time::Sleep};
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[cfg(feature = "metrics")]
use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};
#[cfg(feature = "metrics")]
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};

use std::{
    future::Future,
    io::Write,
    net::SocketAddr,
    pin::{pin, Pin},
    sync::Arc,
    task::{Context, Poll, Waker},
    time::{Duration, Instant, SystemTime},
};

/// Logging target for the file.
const LOG_TARGET: &str = "emissary::runtime::tokio";

#[derive(Default, Clone)]
pub struct Runtime {}

impl Runtime {
    pub fn new() -> Self {
        Self {}
    }
}

pub struct TokioTcpStream(Compat<net::TcpStream>);

impl TokioTcpStream {
    fn new(stream: net::TcpStream) -> Self {
        let stream = TokioAsyncReadCompatExt::compat(stream).into_inner();
        let stream = TokioAsyncWriteCompatExt::compat_write(stream);

        Self(stream)
    }
}

impl AsyncRead for TokioTcpStream {
    #[inline]
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<emissary_core::Result<usize>> {
        let pinned = pin!(&mut self.0);

        match futures::ready!(pinned.poll_read(cx, buf)) {
            Ok(nread) => Poll::Ready(Ok(nread)),
            Err(error) => Poll::Ready(Err(emissary_core::Error::Custom(error.to_string()))),
        }
    }
}

impl AsyncWrite for TokioTcpStream {
    #[inline]
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<emissary_core::Result<usize>> {
        let pinned = pin!(&mut self.0);

        match futures::ready!(pinned.poll_write(cx, buf)) {
            Ok(nwritten) => Poll::Ready(Ok(nwritten)),
            Err(error) => Poll::Ready(Err(emissary_core::Error::Custom(error.to_string()))),
        }
    }

    #[inline]
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<emissary_core::Result<()>> {
        let pinned = pin!(&mut self.0);

        match futures::ready!(pinned.poll_flush(cx)) {
            Ok(()) => Poll::Ready(Ok(())),
            Err(error) => Poll::Ready(Err(emissary_core::Error::Custom(error.to_string()))),
        }
    }

    #[inline]
    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<emissary_core::Result<()>> {
        let pinned = pin!(&mut self.0);

        match futures::ready!(pinned.poll_close(cx)) {
            Ok(()) => Poll::Ready(Ok(())),
            Err(error) => Poll::Ready(Err(emissary_core::Error::Custom(error.to_string()))),
        }
    }
}

impl TcpStream for TokioTcpStream {
    async fn connect(address: SocketAddr) -> Option<Self> {
        match tokio::time::timeout(Duration::from_secs(10), net::TcpStream::connect(address)).await
        {
            Err(_) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?address,
                    "timeout while dialing address",
                );
                None
            }
            Ok(Err(error)) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?address,
                    error = ?error.kind(),
                    "failed to connect"
                );
                None
            }
            Ok(Ok(stream)) => {
                stream.set_nodelay(true).ok()?;

                Some(Self::new(stream))
            }
        }
    }
}

pub struct TokioTcpListener(net::TcpListener);

impl TcpListener<TokioTcpStream> for TokioTcpListener {
    // TODO: can be made sync with `socket2`
    async fn bind(address: SocketAddr) -> Option<Self> {
        net::TcpListener::bind(&address)
            .await
            .map_err(|error| {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?address,
                    error = ?error.kind(),
                    "failed to bind"
                );
            })
            .ok()
            .map(TokioTcpListener)
    }

    fn poll_accept(&mut self, cx: &mut Context<'_>) -> Poll<Option<(TokioTcpStream, SocketAddr)>> {
        loop {
            match futures::ready!(self.0.poll_accept(cx)) {
                Err(_) => return Poll::Ready(None),
                Ok((stream, address)) => match stream.set_nodelay(true) {
                    Err(error) => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            ?error,
                            "failed to configure `TCP_NODELAY` for inbound connection",
                        );
                        continue;
                    }
                    Ok(()) => {
                        return Poll::Ready(Some((TokioTcpStream::new(stream), address)));
                    }
                },
            }
        }
    }

    fn local_address(&self) -> Option<SocketAddr> {
        self.0.local_addr().ok()
    }
}

#[derive(Clone)]
pub struct TokioUdpSocket(Arc<net::UdpSocket>);

impl UdpSocket for TokioUdpSocket {
    fn bind(address: SocketAddr) -> impl Future<Output = Option<Self>> {
        async move { net::UdpSocket::bind(address).await.ok().map(|socket| Self(Arc::new(socket))) }
    }

    #[inline]
    fn send_to(&mut self, buf: &[u8], target: SocketAddr) -> impl Future<Output = Option<usize>> {
        async move { self.0.send_to(buf, target).await.ok() }
    }

    #[inline]
    fn recv_from(&mut self, buf: &mut [u8]) -> impl Future<Output = Option<(usize, SocketAddr)>> {
        async move { self.0.recv_from(buf).await.ok() }
    }

    fn local_address(&self) -> Option<SocketAddr> {
        self.0.local_addr().ok()
    }
}

pub struct TokioJoinSet<T>(task::JoinSet<T>, Option<Waker>);

impl<T: Send + 'static> JoinSet<T> for TokioJoinSet<T> {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn push<F>(&mut self, future: F)
    where
        F: Future<Output = T> + Send + 'static,
        F::Output: Send,
    {
        let _ = self.0.spawn(future);

        // TODO: remove?
        if let Some(waker) = self.1.take() {
            waker.wake_by_ref();
        }
    }
}

impl<T: Send + 'static> Stream for TokioJoinSet<T> {
    type Item = T;

    #[inline]
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.0.poll_join_next(cx) {
            Poll::Pending | Poll::Ready(None) => {
                self.1 = Some(cx.waker().clone());
                Poll::Pending
            }
            Poll::Ready(Some(Err(_))) => Poll::Ready(None),
            Poll::Ready(Some(Ok(value))) => Poll::Ready(Some(value)),
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub struct TokioInstant(Instant);

impl InstantT for TokioInstant {
    #[inline]
    fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }
}

#[derive(Clone)]
#[allow(unused)]
struct TokioMetricsCounter(&'static str);

impl Counter for TokioMetricsCounter {
    #[cfg(feature = "metrics")]
    #[inline]
    fn increment(&mut self, value: usize) {
        counter!(self.0).increment(value as u64);
    }

    #[cfg(not(feature = "metrics"))]
    fn increment(&mut self, _: usize) {}
}

#[derive(Clone)]
#[allow(unused)]
struct TokioMetricsGauge(&'static str);

impl Gauge for TokioMetricsGauge {
    #[cfg(feature = "metrics")]
    #[inline]
    fn increment(&mut self, value: usize) {
        gauge!(self.0).increment(value as f64);
    }

    #[cfg(feature = "metrics")]
    #[inline]
    fn decrement(&mut self, value: usize) {
        gauge!(self.0).decrement(value as f64);
    }

    #[cfg(not(feature = "metrics"))]
    fn increment(&mut self, _: usize) {}

    #[cfg(not(feature = "metrics"))]
    fn decrement(&mut self, _: usize) {}
}

#[derive(Clone)]
#[allow(unused)]
struct TokioMetricsHistogram(&'static str);

impl Histogram for TokioMetricsHistogram {
    #[cfg(feature = "metrics")]
    fn record(&mut self, record: f64) {
        histogram!(self.0).record(record);
    }

    #[cfg(not(feature = "metrics"))]
    fn record(&mut self, _: f64) {}
}

#[derive(Clone)]
pub struct TokioMetricsHandle;

impl MetricsHandle for TokioMetricsHandle {
    #[inline]
    fn counter(&self, name: &'static str) -> impl Counter {
        TokioMetricsCounter(name)
    }

    #[inline]
    fn gauge(&self, name: &'static str) -> impl Gauge {
        TokioMetricsGauge(name)
    }

    #[inline]
    fn histogram(&self, name: &'static str) -> impl Histogram {
        TokioMetricsHistogram(name)
    }
}

impl RuntimeT for Runtime {
    type TcpStream = TokioTcpStream;
    type UdpSocket = TokioUdpSocket;
    type TcpListener = TokioTcpListener;
    type JoinSet<T: Send + 'static> = TokioJoinSet<T>;
    type MetricsHandle = TokioMetricsHandle;
    type Instant = TokioInstant;
    type Timer = Pin<Box<Sleep>>;

    #[inline]
    fn spawn<F>(future: F)
    where
        F: Future + Send + 'static,
        F::Output: Send,
    {
        tokio::spawn(future);
    }

    #[inline]
    fn time_since_epoch() -> Duration {
        SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("to succeed")
    }

    #[inline]
    fn now() -> Self::Instant {
        TokioInstant(Instant::now())
    }

    #[inline]
    fn rng() -> impl RngCore + CryptoRng {
        rand_core::OsRng
    }

    #[inline]
    fn join_set<T: Send + 'static>() -> Self::JoinSet<T> {
        TokioJoinSet(task::JoinSet::<T>::new(), None)
    }

    #[cfg(feature = "metrics")]
    fn register_metrics(metrics: Vec<MetricType>, port: Option<u16>) -> Self::MetricsHandle {
        if metrics.is_empty() {
            tracing::info!(
                target: LOG_TARGET,
                "disabling metrics server",
            );

            return TokioMetricsHandle {};
        }

        let address = format!("0.0.0.0:{}", port.unwrap_or(12842));
        let builder =
            PrometheusBuilder::new().with_http_listener(address.parse::<SocketAddr>().expect(""));

        tracing::info!(
            target: LOG_TARGET,
            ?address,
            "starting prometheus server",
        );

        metrics
            .into_iter()
            .fold(builder, |builder, metric| match metric {
                MetricType::Counter { name, description } => {
                    describe_counter!(name, description);
                    builder
                }
                MetricType::Gauge { name, description } => {
                    describe_gauge!(name, description);
                    builder
                }
                MetricType::Histogram {
                    name,
                    description,
                    buckets,
                } => {
                    describe_histogram!(name, description);
                    builder
                        .set_buckets_for_metric(Matcher::Full(name.to_string()), &buckets)
                        .expect("to succeed")
                }
            })
            .install()
            .expect("to succeed");

        TokioMetricsHandle {}
    }

    #[cfg(not(feature = "metrics"))]
    fn register_metrics(_: Vec<MetricType>, _: Option<u16>) -> Self::MetricsHandle {
        TokioMetricsHandle {}
    }

    #[inline]
    fn timer(duration: Duration) -> Self::Timer {
        Box::pin(tokio::time::sleep(duration))
    }

    #[inline]
    async fn delay(duration: Duration) {
        tokio::time::sleep(duration).await
    }

    #[inline]
    fn gzip_compress(bytes: impl AsRef<[u8]>) -> Option<Vec<u8>> {
        let mut e = GzEncoder::new(Vec::new(), Compression::default());
        e.write_all(bytes.as_ref()).ok()?;

        e.finish().ok()
    }

    #[inline]
    fn gzip_decompress(bytes: impl AsRef<[u8]>) -> Option<Vec<u8>> {
        let mut e = GzDecoder::new(Vec::new());
        e.write_all(bytes.as_ref()).ok()?;

        e.finish().ok()
    }
}
