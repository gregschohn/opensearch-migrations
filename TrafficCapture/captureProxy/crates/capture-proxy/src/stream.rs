//! Byte streams that may or may not be wrapped in TLS.
//!
//! [`capture_transport::Connection`] is generic over its two streams, and whether each side is
//! TLS is a run-time configuration decision: the frontside is TLS exactly when a certificate chain
//! is configured, and the destination side is TLS exactly when `--destination-uri` has the `https`
//! scheme. Monomorphising the connection over all four combinations would quadruple the data-plane
//! code and still need a run-time branch to pick which instantiation to call, so each side is
//! wrapped in a two-variant enum instead.
//!
//! The branch is per `poll_read`/`poll_write`, not per byte, and it is a direct jump rather than a
//! vtable lookup. Boxing the TLS variants keeps both enums small: `rustls`' connection state is
//! several hundred bytes, and a plaintext deployment should not pay for it in every stack frame
//! that holds a stream.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

/// The client-facing stream: plaintext, or a completed server-side TLS session.
#[derive(Debug)]
pub enum ClientStream {
    /// No frontside TLS is configured.
    Plain(TcpStream),
    /// Frontside TLS, with the handshake already complete.
    Tls(Box<tokio_rustls::server::TlsStream<TcpStream>>),
}

/// The destination-facing stream: plaintext, or a completed client-side TLS session.
#[derive(Debug)]
pub enum SourceStream {
    /// The destination URI used the `http` scheme.
    Plain(TcpStream),
    /// The destination URI used the `https` scheme, with the handshake already complete.
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

/// Generates the three `AsyncRead`/`AsyncWrite` forwards for a two-variant stream enum.
///
/// Both enums are `Unpin` (every variant is), so `Pin::new` on a reborrow is enough and no pin
/// projection — and therefore no `unsafe` — is involved.
macro_rules! forward_stream_io {
    ($name:ident) => {
        impl AsyncRead for $name {
            fn poll_read(
                self: Pin<&mut Self>,
                context: &mut Context<'_>,
                buffer: &mut ReadBuf<'_>,
            ) -> Poll<io::Result<()>> {
                match self.get_mut() {
                    Self::Plain(stream) => Pin::new(stream).poll_read(context, buffer),
                    Self::Tls(stream) => Pin::new(stream.as_mut()).poll_read(context, buffer),
                }
            }
        }

        impl AsyncWrite for $name {
            fn poll_write(
                self: Pin<&mut Self>,
                context: &mut Context<'_>,
                bytes: &[u8],
            ) -> Poll<io::Result<usize>> {
                match self.get_mut() {
                    Self::Plain(stream) => Pin::new(stream).poll_write(context, bytes),
                    Self::Tls(stream) => Pin::new(stream.as_mut()).poll_write(context, bytes),
                }
            }

            fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
                match self.get_mut() {
                    Self::Plain(stream) => Pin::new(stream).poll_flush(context),
                    Self::Tls(stream) => Pin::new(stream.as_mut()).poll_flush(context),
                }
            }

            fn poll_shutdown(
                self: Pin<&mut Self>,
                context: &mut Context<'_>,
            ) -> Poll<io::Result<()>> {
                match self.get_mut() {
                    Self::Plain(stream) => Pin::new(stream).poll_shutdown(context),
                    Self::Tls(stream) => Pin::new(stream.as_mut()).poll_shutdown(context),
                }
            }
        }
    };
}

forward_stream_io!(ClientStream);
forward_stream_io!(SourceStream);

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    /// The plaintext variants must be byte-transparent: the whole no-capture transport gate
    /// depends on this wrapper adding nothing to the stream.
    #[tokio::test]
    async fn the_plaintext_variants_forward_bytes_unchanged() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local address");

        let echo = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut server = ClientStream::Plain(stream);
            let mut received = vec![0_u8; 5];
            server.read_exact(&mut received).await.expect("read");
            server.write_all(&received).await.expect("write");
            server.flush().await.expect("flush");
            received
        });

        let mut client = SourceStream::Plain(TcpStream::connect(address).await.expect("connect"));
        client.write_all(b"hello").await.expect("write");
        let mut echoed = vec![0_u8; 5];
        client.read_exact(&mut echoed).await.expect("read");

        assert_eq!(&echoed, b"hello");
        assert_eq!(&echo.await.expect("echo task"), b"hello");
    }
}
