// Copyright 2020 Parity Technologies (UK) Ltd.
//
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

mod error;

pub(crate) mod pool;

pub use error::{
    ConnectionError, PendingConnectionError, PendingInboundConnectionError,
    PendingOutboundConnectionError,
};

use crate::handler::{
    AddressChange, ConnectionEvent, ConnectionHandler, DialUpgradeError, FullyNegotiatedInbound,
    FullyNegotiatedOutbound, ListenUpgradeError, ProtocolsChange,
};
use crate::upgrade::{InboundUpgradeSend, OutboundUpgradeSend, SendWrapper, UpgradeInfoSend};
use crate::{ConnectionHandlerEvent, ConnectionHandlerUpgrErr, KeepAlive, SubstreamProtocol};
use futures::stream::FuturesUnordered;
use futures::FutureExt;
use futures::StreamExt;
use futures_timer::Delay;
use instant::Instant;
use libp2p_core::connection::ConnectedPoint;
use libp2p_core::multiaddr::Multiaddr;
use libp2p_core::muxing::{StreamMuxerBox, StreamMuxerEvent, StreamMuxerExt, SubstreamBox};
use libp2p_core::upgrade::{InboundUpgradeApply, OutboundUpgradeApply};
use libp2p_core::Endpoint;
use libp2p_core::{upgrade, ProtocolName as _, UpgradeError};
use libp2p_identity::PeerId;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Waker;
use std::time::Duration;
use std::{fmt, io, mem, pin::Pin, task::Context, task::Poll};

static NEXT_CONNECTION_ID: AtomicUsize = AtomicUsize::new(1);

/// Connection identifier.
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct ConnectionId(usize);

impl ConnectionId {
    /// A "dummy" [`ConnectionId`].
    ///
    /// Really, you should not use this, not even for testing but it is here if you need it.
    #[deprecated(
        since = "0.42.0",
        note = "Don't use this, it will be removed at a later stage again."
    )]
    pub const DUMMY: ConnectionId = ConnectionId(0);

    /// Creates an _unchecked_ [`ConnectionId`].
    ///
    /// [`Swarm`](crate::Swarm) enforces that [`ConnectionId`]s are unique and not reused.
    /// This constructor does not, hence the _unchecked_.
    ///
    /// It is primarily meant for allowing manual tests of [`NetworkBehaviour`](crate::NetworkBehaviour)s.
    pub fn new_unchecked(id: usize) -> Self {
        Self(id)
    }

    /// Returns the next available [`ConnectionId`].
    pub(crate) fn next() -> Self {
        Self(NEXT_CONNECTION_ID.fetch_add(1, Ordering::SeqCst))
    }
}

/// Information about a successfully established connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connected {
    /// The connected endpoint, including network address information.
    pub endpoint: ConnectedPoint,
    /// Information obtained from the transport.
    pub peer_id: PeerId,
}

/// Event generated by a [`Connection`].
#[derive(Debug, Clone)]
pub enum Event<T> {
    /// Event generated by the [`ConnectionHandler`].
    Handler(T),
    /// Address of the remote has changed.
    AddressChange(Multiaddr),
}

/// A multiplexed connection to a peer with an associated [`ConnectionHandler`].
pub struct Connection<THandler>
where
    THandler: ConnectionHandler,
{
    /// Node that handles the muxing.
    muxing: StreamMuxerBox,
    /// The underlying handler.
    handler: THandler,
    /// Futures that upgrade incoming substreams.
    negotiating_in: FuturesUnordered<
        SubstreamUpgrade<
            THandler::InboundOpenInfo,
            InboundUpgradeApply<SubstreamBox, SendWrapper<THandler::InboundProtocol>>,
        >,
    >,
    /// Futures that upgrade outgoing substreams.
    negotiating_out: FuturesUnordered<
        SubstreamUpgrade<
            THandler::OutboundOpenInfo,
            OutboundUpgradeApply<SubstreamBox, SendWrapper<THandler::OutboundProtocol>>,
        >,
    >,
    /// The currently planned connection & handler shutdown.
    shutdown: Shutdown,
    /// The substream upgrade protocol override, if any.
    substream_upgrade_protocol_override: Option<upgrade::Version>,
    /// The maximum number of inbound streams concurrently negotiating on a
    /// connection. New inbound streams exceeding the limit are dropped and thus
    /// reset.
    ///
    /// Note: This only enforces a limit on the number of concurrently
    /// negotiating inbound streams. The total number of inbound streams on a
    /// connection is the sum of negotiating and negotiated streams. A limit on
    /// the total number of streams can be enforced at the
    /// [`StreamMuxerBox`](libp2p_core::muxing::StreamMuxerBox) level.
    max_negotiating_inbound_streams: usize,
    /// Contains all upgrades that are waiting for a new outbound substream.
    ///
    /// The upgrade timeout is already ticking here so this may fail in case the remote is not quick
    /// enough in providing us with a new stream.
    requested_substreams: FuturesUnordered<
        SubstreamRequested<THandler::OutboundOpenInfo, THandler::OutboundProtocol>,
    >,

    supported_protocols: Vec<String>,
}

impl<THandler> fmt::Debug for Connection<THandler>
where
    THandler: ConnectionHandler + fmt::Debug,
    THandler::OutboundOpenInfo: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connection")
            .field("handler", &self.handler)
            .finish()
    }
}

impl<THandler> Unpin for Connection<THandler> where THandler: ConnectionHandler {}

impl<THandler> Connection<THandler>
where
    THandler: ConnectionHandler,
{
    /// Builds a new `Connection` from the given substream multiplexer
    /// and connection handler.
    pub fn new(
        muxer: StreamMuxerBox,
        handler: THandler,
        substream_upgrade_protocol_override: Option<upgrade::Version>,
        max_negotiating_inbound_streams: usize,
    ) -> Self {
        Connection {
            muxing: muxer,
            handler,
            negotiating_in: Default::default(),
            negotiating_out: Default::default(),
            shutdown: Shutdown::None,
            substream_upgrade_protocol_override,
            max_negotiating_inbound_streams,
            requested_substreams: Default::default(),
            supported_protocols: vec![],
        }
    }

    /// Notifies the connection handler of an event.
    pub fn on_behaviour_event(&mut self, event: THandler::InEvent) {
        self.handler.on_behaviour_event(event);
    }

    /// Begins an orderly shutdown of the connection, returning the connection
    /// handler and a `Future` that resolves when connection shutdown is complete.
    pub fn close(self) -> (THandler, impl Future<Output = io::Result<()>>) {
        (self.handler, self.muxing.close())
    }

    /// Polls the handler and the substream, forwarding events from the former to the latter and
    /// vice versa.
    pub fn poll(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Event<THandler::OutEvent>, ConnectionError<THandler::Error>>> {
        let Self {
            requested_substreams,
            muxing,
            handler,
            negotiating_out,
            negotiating_in,
            shutdown,
            max_negotiating_inbound_streams,
            substream_upgrade_protocol_override,
            supported_protocols,
        } = self.get_mut();

        loop {
            match requested_substreams.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(()))) => continue,
                Poll::Ready(Some(Err(info))) => {
                    handler.on_connection_event(ConnectionEvent::DialUpgradeError(
                        DialUpgradeError {
                            info,
                            error: ConnectionHandlerUpgrErr::Timeout,
                        },
                    ));
                    continue;
                }
                Poll::Ready(None) | Poll::Pending => {}
            }

            // Poll the [`ConnectionHandler`].
            match handler.poll(cx) {
                Poll::Pending => {}
                Poll::Ready(ConnectionHandlerEvent::OutboundSubstreamRequest { protocol }) => {
                    let timeout = *protocol.timeout();
                    let (upgrade, user_data) = protocol.into_upgrade();

                    requested_substreams.push(SubstreamRequested::new(user_data, timeout, upgrade));
                    continue; // Poll handler until exhausted.
                }
                Poll::Ready(ConnectionHandlerEvent::Custom(event)) => {
                    return Poll::Ready(Ok(Event::Handler(event)));
                }
                Poll::Ready(ConnectionHandlerEvent::Close(err)) => {
                    return Poll::Ready(Err(ConnectionError::Handler(err)));
                }
            }

            // In case the [`ConnectionHandler`] can not make any more progress, poll the negotiating outbound streams.
            match negotiating_out.poll_next_unpin(cx) {
                Poll::Pending | Poll::Ready(None) => {}
                Poll::Ready(Some((info, Ok(protocol)))) => {
                    handler.on_connection_event(ConnectionEvent::FullyNegotiatedOutbound(
                        FullyNegotiatedOutbound { protocol, info },
                    ));
                    continue;
                }
                Poll::Ready(Some((info, Err(error)))) => {
                    handler.on_connection_event(ConnectionEvent::DialUpgradeError(
                        DialUpgradeError { info, error },
                    ));
                    continue;
                }
            }

            // In case both the [`ConnectionHandler`] and the negotiating outbound streams can not
            // make any more progress, poll the negotiating inbound streams.
            match negotiating_in.poll_next_unpin(cx) {
                Poll::Pending | Poll::Ready(None) => {}
                Poll::Ready(Some((info, Ok(protocol)))) => {
                    handler.on_connection_event(ConnectionEvent::FullyNegotiatedInbound(
                        FullyNegotiatedInbound { protocol, info },
                    ));
                    continue;
                }
                Poll::Ready(Some((info, Err(error)))) => {
                    handler.on_connection_event(ConnectionEvent::ListenUpgradeError(
                        ListenUpgradeError { info, error },
                    ));
                    continue;
                }
            }

            // Ask the handler whether it wants the connection (and the handler itself)
            // to be kept alive, which determines the planned shutdown, if any.
            let keep_alive = handler.connection_keep_alive();
            match (&mut *shutdown, keep_alive) {
                (Shutdown::Later(timer, deadline), KeepAlive::Until(t)) => {
                    if *deadline != t {
                        *deadline = t;
                        if let Some(dur) = deadline.checked_duration_since(Instant::now()) {
                            timer.reset(dur)
                        }
                    }
                }
                (_, KeepAlive::Until(t)) => {
                    if let Some(dur) = t.checked_duration_since(Instant::now()) {
                        *shutdown = Shutdown::Later(Delay::new(dur), t)
                    }
                }
                (_, KeepAlive::No) => *shutdown = Shutdown::Asap,
                (_, KeepAlive::Yes) => *shutdown = Shutdown::None,
            };

            // Check if the connection (and handler) should be shut down.
            // As long as we're still negotiating substreams, shutdown is always postponed.
            if negotiating_in.is_empty()
                && negotiating_out.is_empty()
                && requested_substreams.is_empty()
            {
                match shutdown {
                    Shutdown::None => {}
                    Shutdown::Asap => return Poll::Ready(Err(ConnectionError::KeepAliveTimeout)),
                    Shutdown::Later(delay, _) => match Future::poll(Pin::new(delay), cx) {
                        Poll::Ready(_) => {
                            return Poll::Ready(Err(ConnectionError::KeepAliveTimeout))
                        }
                        Poll::Pending => {}
                    },
                }
            }

            match muxing.poll_unpin(cx)? {
                Poll::Pending => {}
                Poll::Ready(StreamMuxerEvent::AddressChange(address)) => {
                    handler.on_connection_event(ConnectionEvent::AddressChange(AddressChange {
                        new_address: &address,
                    }));
                    return Poll::Ready(Ok(Event::AddressChange(address)));
                }
            }

            if let Some(requested_substream) = requested_substreams.iter_mut().next() {
                match muxing.poll_outbound_unpin(cx)? {
                    Poll::Pending => {}
                    Poll::Ready(substream) => {
                        let (user_data, timeout, upgrade) = requested_substream.extract();

                        negotiating_out.push(SubstreamUpgrade::new_outbound(
                            substream,
                            user_data,
                            timeout,
                            upgrade,
                            *substream_upgrade_protocol_override,
                        ));

                        continue; // Go back to the top, handler can potentially make progress again.
                    }
                }
            }

            if negotiating_in.len() < *max_negotiating_inbound_streams {
                match muxing.poll_inbound_unpin(cx)? {
                    Poll::Pending => {}
                    Poll::Ready(substream) => {
                        let protocol = handler.listen_protocol();

                        let mut new_protocols = protocol
                            .upgrade()
                            .protocol_info()
                            .filter_map(|i| String::from_utf8(i.protocol_name().to_vec()).ok())
                            .collect::<Vec<_>>();

                        new_protocols.sort();

                        if supported_protocols != &new_protocols {
                            handler.on_connection_event(ConnectionEvent::ProtocolsChange(
                                ProtocolsChange {
                                    protocols: &new_protocols,
                                },
                            ));
                            *supported_protocols = new_protocols;
                        }

                        negotiating_in.push(SubstreamUpgrade::new_inbound(substream, protocol));

                        continue; // Go back to the top, handler can potentially make progress again.
                    }
                }
            }

            return Poll::Pending; // Nothing can make progress, return `Pending`.
        }
    }
}

/// Borrowed information about an incoming connection currently being negotiated.
#[derive(Debug, Copy, Clone)]
pub struct IncomingInfo<'a> {
    /// Local connection address.
    pub local_addr: &'a Multiaddr,
    /// Address used to send back data to the remote.
    pub send_back_addr: &'a Multiaddr,
}

impl<'a> IncomingInfo<'a> {
    /// Builds the [`ConnectedPoint`] corresponding to the incoming connection.
    pub fn create_connected_point(&self) -> ConnectedPoint {
        ConnectedPoint::Listener {
            local_addr: self.local_addr.clone(),
            send_back_addr: self.send_back_addr.clone(),
        }
    }
}

/// Information about a connection limit.
#[deprecated(note = "Use `libp2p::connection_limits` instead.", since = "0.42.1")]
#[derive(Debug, Clone, Copy)]
pub struct ConnectionLimit {
    /// The maximum number of connections.
    pub limit: u32,
    /// The current number of connections.
    pub current: u32,
}

#[allow(deprecated)]
impl fmt::Display for ConnectionLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "connection limit exceeded ({}/{})",
            self.current, self.limit
        )
    }
}

/// A `ConnectionLimit` can represent an error if it has been exceeded.
#[allow(deprecated)]
impl std::error::Error for ConnectionLimit {}

struct SubstreamUpgrade<UserData, Upgrade> {
    user_data: Option<UserData>,
    timeout: Delay,
    upgrade: Upgrade,
}

impl<UserData, Upgrade>
    SubstreamUpgrade<UserData, OutboundUpgradeApply<SubstreamBox, SendWrapper<Upgrade>>>
where
    Upgrade: Send + OutboundUpgradeSend,
{
    fn new_outbound(
        substream: SubstreamBox,
        user_data: UserData,
        timeout: Delay,
        upgrade: Upgrade,
        version_override: Option<upgrade::Version>,
    ) -> Self {
        let effective_version = match version_override {
            Some(version_override) if version_override != upgrade::Version::default() => {
                log::debug!(
                    "Substream upgrade protocol override: {:?} -> {:?}",
                    upgrade::Version::default(),
                    version_override
                );

                version_override
            }
            _ => upgrade::Version::default(),
        };

        Self {
            user_data: Some(user_data),
            timeout,
            upgrade: upgrade::apply_outbound(substream, SendWrapper(upgrade), effective_version),
        }
    }
}

impl<UserData, Upgrade>
    SubstreamUpgrade<UserData, InboundUpgradeApply<SubstreamBox, SendWrapper<Upgrade>>>
where
    Upgrade: Send + InboundUpgradeSend,
{
    fn new_inbound(
        substream: SubstreamBox,
        protocol: SubstreamProtocol<Upgrade, UserData>,
    ) -> Self {
        let timeout = *protocol.timeout();
        let (upgrade, open_info) = protocol.into_upgrade();

        Self {
            user_data: Some(open_info),
            timeout: Delay::new(timeout),
            upgrade: upgrade::apply_inbound(substream, SendWrapper(upgrade)),
        }
    }
}

impl<UserData, Upgrade> Unpin for SubstreamUpgrade<UserData, Upgrade> {}

impl<UserData, Upgrade, UpgradeOutput, TUpgradeError> Future for SubstreamUpgrade<UserData, Upgrade>
where
    Upgrade: Future<Output = Result<UpgradeOutput, UpgradeError<TUpgradeError>>> + Unpin,
{
    type Output = (
        UserData,
        Result<UpgradeOutput, ConnectionHandlerUpgrErr<TUpgradeError>>,
    );

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        match self.timeout.poll_unpin(cx) {
            Poll::Ready(()) => {
                return Poll::Ready((
                    self.user_data
                        .take()
                        .expect("Future not to be polled again once ready."),
                    Err(ConnectionHandlerUpgrErr::Timeout),
                ))
            }

            Poll::Pending => {}
        }

        match self.upgrade.poll_unpin(cx) {
            Poll::Ready(Ok(upgrade)) => Poll::Ready((
                self.user_data
                    .take()
                    .expect("Future not to be polled again once ready."),
                Ok(upgrade),
            )),
            Poll::Ready(Err(err)) => Poll::Ready((
                self.user_data
                    .take()
                    .expect("Future not to be polled again once ready."),
                Err(ConnectionHandlerUpgrErr::Upgrade(err)),
            )),
            Poll::Pending => Poll::Pending,
        }
    }
}

enum SubstreamRequested<UserData, Upgrade> {
    Waiting {
        user_data: UserData,
        timeout: Delay,
        upgrade: Upgrade,
        /// A waker to notify our [`FuturesUnordered`] that we have extracted the data.
        ///
        /// This will ensure that we will get polled again in the next iteration which allows us to
        /// resolve with `Ok(())` and be removed from the [`FuturesUnordered`].
        extracted_waker: Option<Waker>,
    },
    Done,
}

impl<UserData, Upgrade> SubstreamRequested<UserData, Upgrade> {
    fn new(user_data: UserData, timeout: Duration, upgrade: Upgrade) -> Self {
        Self::Waiting {
            user_data,
            timeout: Delay::new(timeout),
            upgrade,
            extracted_waker: None,
        }
    }

    fn extract(&mut self) -> (UserData, Delay, Upgrade) {
        match mem::replace(self, Self::Done) {
            SubstreamRequested::Waiting {
                user_data,
                timeout,
                upgrade,
                extracted_waker: waker,
            } => {
                if let Some(waker) = waker {
                    waker.wake();
                }

                (user_data, timeout, upgrade)
            }
            SubstreamRequested::Done => panic!("cannot extract twice"),
        }
    }
}

impl<UserData, Upgrade> Unpin for SubstreamRequested<UserData, Upgrade> {}

impl<UserData, Upgrade> Future for SubstreamRequested<UserData, Upgrade> {
    type Output = Result<(), UserData>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        match mem::replace(this, Self::Done) {
            SubstreamRequested::Waiting {
                user_data,
                upgrade,
                mut timeout,
                ..
            } => match timeout.poll_unpin(cx) {
                Poll::Ready(()) => Poll::Ready(Err(user_data)),
                Poll::Pending => {
                    *this = Self::Waiting {
                        user_data,
                        upgrade,
                        timeout,
                        extracted_waker: Some(cx.waker().clone()),
                    };
                    Poll::Pending
                }
            },
            SubstreamRequested::Done => Poll::Ready(Ok(())),
        }
    }
}

/// The options for a planned connection & handler shutdown.
///
/// A shutdown is planned anew based on the the return value of
/// [`ConnectionHandler::connection_keep_alive`] of the underlying handler
/// after every invocation of [`ConnectionHandler::poll`].
///
/// A planned shutdown is always postponed for as long as there are ingoing
/// or outgoing substreams being negotiated, i.e. it is a graceful, "idle"
/// shutdown.
#[derive(Debug)]
enum Shutdown {
    /// No shutdown is planned.
    None,
    /// A shut down is planned as soon as possible.
    Asap,
    /// A shut down is planned for when a `Delay` has elapsed.
    Later(Delay, Instant),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keep_alive;
    use futures::future;
    use futures::AsyncRead;
    use futures::AsyncWrite;
    use libp2p_core::upgrade::{DeniedUpgrade, InboundUpgrade, OutboundUpgrade, UpgradeInfo};
    use libp2p_core::StreamMuxer;
    use quickcheck::*;
    use std::sync::{Arc, Weak};
    use void::Void;

    #[test]
    fn max_negotiating_inbound_streams() {
        fn prop(max_negotiating_inbound_streams: u8) {
            let max_negotiating_inbound_streams: usize = max_negotiating_inbound_streams.into();

            let alive_substream_counter = Arc::new(());

            let mut connection = Connection::new(
                StreamMuxerBox::new(DummyStreamMuxer {
                    counter: alive_substream_counter.clone(),
                }),
                keep_alive::ConnectionHandler,
                None,
                max_negotiating_inbound_streams,
            );

            let result = Pin::new(&mut connection)
                .poll(&mut Context::from_waker(futures::task::noop_waker_ref()));

            assert!(result.is_pending());
            assert_eq!(
                Arc::weak_count(&alive_substream_counter),
                max_negotiating_inbound_streams,
                "Expect no more than the maximum number of allowed streams"
            );
        }

        QuickCheck::new().quickcheck(prop as fn(_));
    }

    #[test]
    fn outbound_stream_timeout_starts_on_request() {
        let upgrade_timeout = Duration::from_secs(1);
        let mut connection = Connection::new(
            StreamMuxerBox::new(PendingStreamMuxer),
            MockConnectionHandler::new(upgrade_timeout),
            None,
            2,
        );

        connection.handler.open_new_outbound();
        let _ = Pin::new(&mut connection)
            .poll(&mut Context::from_waker(futures::task::noop_waker_ref()));

        std::thread::sleep(upgrade_timeout + Duration::from_secs(1));

        let _ = Pin::new(&mut connection)
            .poll(&mut Context::from_waker(futures::task::noop_waker_ref()));

        assert!(matches!(
            connection.handler.error.unwrap(),
            ConnectionHandlerUpgrErr::Timeout
        ))
    }

    #[test]
    fn propagates_changes_to_supported_inbound_protocols() {
        let mut connection = Connection::new(
            StreamMuxerBox::new(DummyStreamMuxer {
                counter: Arc::new(()),
            }),
            ConfigurableProtocolConnectionHandler::default(),
            None,
            2,
        );
        connection.handler.active_protocols = vec!["/foo"];

        // DummyStreamMuxer will yield a new stream
        let _ = Pin::new(&mut connection)
            .poll(&mut Context::from_waker(futures::task::noop_waker_ref()));
        assert_eq!(connection.handler.reported_protocols, vec!["/foo"]);

        connection.handler.active_protocols = vec!["/foo", "/bar"];
        connection.negotiating_in.clear(); // Hack to request more substreams from the muxer.

        // DummyStreamMuxer will yield a new stream
        let _ = Pin::new(&mut connection)
            .poll(&mut Context::from_waker(futures::task::noop_waker_ref()));

        assert_eq!(connection.handler.reported_protocols, vec!["/bar", "/foo"])
    }

    struct DummyStreamMuxer {
        counter: Arc<()>,
    }

    impl StreamMuxer for DummyStreamMuxer {
        type Substream = PendingSubstream;
        type Error = Void;

        fn poll_inbound(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Result<Self::Substream, Self::Error>> {
            Poll::Ready(Ok(PendingSubstream(Arc::downgrade(&self.counter))))
        }

        fn poll_outbound(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Result<Self::Substream, Self::Error>> {
            Poll::Pending
        }

        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Result<StreamMuxerEvent, Self::Error>> {
            Poll::Pending
        }
    }

    /// A [`StreamMuxer`] which never returns a stream.
    struct PendingStreamMuxer;

    impl StreamMuxer for PendingStreamMuxer {
        type Substream = PendingSubstream;
        type Error = Void;

        fn poll_inbound(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Result<Self::Substream, Self::Error>> {
            Poll::Pending
        }

        fn poll_outbound(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Result<Self::Substream, Self::Error>> {
            Poll::Pending
        }

        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Pending
        }

        fn poll(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Result<StreamMuxerEvent, Self::Error>> {
            Poll::Pending
        }
    }

    struct PendingSubstream(Weak<()>);

    impl AsyncRead for PendingSubstream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for PendingSubstream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Pending
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }
    }

    struct MockConnectionHandler {
        outbound_requested: bool,
        error: Option<ConnectionHandlerUpgrErr<Void>>,
        upgrade_timeout: Duration,
    }

    impl MockConnectionHandler {
        fn new(upgrade_timeout: Duration) -> Self {
            Self {
                outbound_requested: false,
                error: None,
                upgrade_timeout,
            }
        }

        fn open_new_outbound(&mut self) {
            self.outbound_requested = true;
        }
    }

    #[derive(Default)]
    struct ConfigurableProtocolConnectionHandler {
        active_protocols: Vec<&'static str>,
        reported_protocols: Vec<String>,
    }

    impl ConnectionHandler for MockConnectionHandler {
        type InEvent = Void;
        type OutEvent = Void;
        type Error = Void;
        type InboundProtocol = DeniedUpgrade;
        type OutboundProtocol = DeniedUpgrade;
        type InboundOpenInfo = ();
        type OutboundOpenInfo = ();

        fn listen_protocol(
            &self,
        ) -> SubstreamProtocol<Self::InboundProtocol, Self::InboundOpenInfo> {
            SubstreamProtocol::new(DeniedUpgrade, ()).with_timeout(self.upgrade_timeout)
        }

        fn on_connection_event(
            &mut self,
            event: ConnectionEvent<
                Self::InboundProtocol,
                Self::OutboundProtocol,
                Self::InboundOpenInfo,
                Self::OutboundOpenInfo,
            >,
        ) {
            match event {
                ConnectionEvent::FullyNegotiatedInbound(FullyNegotiatedInbound {
                    protocol,
                    ..
                }) => void::unreachable(protocol),
                ConnectionEvent::FullyNegotiatedOutbound(FullyNegotiatedOutbound {
                    protocol,
                    ..
                }) => void::unreachable(protocol),
                ConnectionEvent::DialUpgradeError(DialUpgradeError { error, .. }) => {
                    self.error = Some(error)
                }
                ConnectionEvent::AddressChange(_)
                | ConnectionEvent::ListenUpgradeError(_)
                | ConnectionEvent::ProtocolsChange(_) => {}
            }
        }

        fn on_behaviour_event(&mut self, event: Self::InEvent) {
            void::unreachable(event)
        }

        fn connection_keep_alive(&self) -> KeepAlive {
            KeepAlive::Yes
        }

        fn poll(
            &mut self,
            _: &mut Context<'_>,
        ) -> Poll<
            ConnectionHandlerEvent<
                Self::OutboundProtocol,
                Self::OutboundOpenInfo,
                Self::OutEvent,
                Self::Error,
            >,
        > {
            if self.outbound_requested {
                self.outbound_requested = false;
                return Poll::Ready(ConnectionHandlerEvent::OutboundSubstreamRequest {
                    protocol: SubstreamProtocol::new(DeniedUpgrade, ())
                        .with_timeout(self.upgrade_timeout),
                });
            }

            Poll::Pending
        }
    }

    impl ConnectionHandler for ConfigurableProtocolConnectionHandler {
        type InEvent = Void;
        type OutEvent = Void;
        type Error = Void;
        type InboundProtocol = ManyProtocolsUpgrade;
        type OutboundProtocol = DeniedUpgrade;
        type InboundOpenInfo = ();
        type OutboundOpenInfo = ();

        fn listen_protocol(
            &self,
        ) -> SubstreamProtocol<Self::InboundProtocol, Self::InboundOpenInfo> {
            SubstreamProtocol::new(
                ManyProtocolsUpgrade {
                    protocols: self.active_protocols.clone(),
                },
                (),
            )
        }

        fn on_connection_event(
            &mut self,
            event: ConnectionEvent<
                Self::InboundProtocol,
                Self::OutboundProtocol,
                Self::InboundOpenInfo,
                Self::OutboundOpenInfo,
            >,
        ) {
            if let ConnectionEvent::ProtocolsChange(ProtocolsChange { protocols }) = event {
                self.reported_protocols = protocols
                    .to_vec();
            }
        }

        fn on_behaviour_event(&mut self, event: Self::InEvent) {
            void::unreachable(event)
        }

        fn connection_keep_alive(&self) -> KeepAlive {
            KeepAlive::Yes
        }

        fn poll(
            &mut self,
            _: &mut Context<'_>,
        ) -> Poll<
            ConnectionHandlerEvent<
                Self::OutboundProtocol,
                Self::OutboundOpenInfo,
                Self::OutEvent,
                Self::Error,
            >,
        > {
            Poll::Pending
        }
    }

    struct ManyProtocolsUpgrade {
        protocols: Vec<&'static str>,
    }

    impl UpgradeInfo for ManyProtocolsUpgrade {
        type Info = &'static str;
        type InfoIter = std::vec::IntoIter<Self::Info>;

        fn protocol_info(&self) -> Self::InfoIter {
            self.protocols.clone().into_iter()
        }
    }

    impl<C> InboundUpgrade<C> for ManyProtocolsUpgrade {
        type Output = C;
        type Error = Void;
        type Future = future::Ready<Result<Self::Output, Self::Error>>;

        fn upgrade_inbound(self, stream: C, _: Self::Info) -> Self::Future {
            future::ready(Ok(stream))
        }
    }

    impl<C> OutboundUpgrade<C> for ManyProtocolsUpgrade {
        type Output = C;
        type Error = Void;
        type Future = future::Ready<Result<Self::Output, Self::Error>>;

        fn upgrade_outbound(self, stream: C, _: Self::Info) -> Self::Future {
            future::ready(Ok(stream))
        }
    }
}

/// The endpoint roles associated with a pending peer-to-peer connection.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum PendingPoint {
    /// The socket comes from a dialer.
    ///
    /// There is no single address associated with the Dialer of a pending
    /// connection. Addresses are dialed in parallel. Only once the first dial
    /// is successful is the address of the connection known.
    Dialer {
        /// Same as [`ConnectedPoint::Dialer`] `role_override`.
        role_override: Endpoint,
    },
    /// The socket comes from a listener.
    Listener {
        /// Local connection address.
        local_addr: Multiaddr,
        /// Address used to send back data to the remote.
        send_back_addr: Multiaddr,
    },
}

impl From<ConnectedPoint> for PendingPoint {
    fn from(endpoint: ConnectedPoint) -> Self {
        match endpoint {
            ConnectedPoint::Dialer { role_override, .. } => PendingPoint::Dialer { role_override },
            ConnectedPoint::Listener {
                local_addr,
                send_back_addr,
            } => PendingPoint::Listener {
                local_addr,
                send_back_addr,
            },
        }
    }
}
