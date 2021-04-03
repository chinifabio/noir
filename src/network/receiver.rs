use std::time::Duration;

use anyhow::{anyhow, Result};

use crate::channel::{
    BoundedChannelReceiver, BoundedChannelSender, RecvTimeoutError, SelectAnyResult, SelectResult,
};
use crate::network::{NetworkSender, ReceiverEndpoint};
use crate::operator::Data;

/// The capacity of the in-buffer.
const CHANNEL_CAPACITY: usize = 10;

/// The receiving end of a connection between two replicas.
///
/// This works for both a local in-memory connection and for a remote socket connection. This will
/// always be able to listen to a socket. No socket will be bound until a message is sent to the
/// starter returned by the constructor.
///
/// Internally it contains a in-memory sender-receiver pair, to get the local sender call
/// `.sender()`. When the socket will be bound an task will be spawned, it will bind the
/// socket and send to the same in-memory channel the received messages.
#[derive(Derivative)]
#[derivative(Debug)]
pub struct NetworkReceiver<In: Data> {
    /// The ReceiverEndpoint of the current receiver.
    pub receiver_endpoint: ReceiverEndpoint,
    /// The actual receiver where the users of this struct will wait upon.
    #[derivative(Debug = "ignore")]
    receiver: BoundedChannelReceiver<In>,
    /// The sender associated with `self.receiver`.
    #[derivative(Debug = "ignore")]
    local_sender: Option<BoundedChannelSender<In>>,
}

impl<In: Data> NetworkReceiver<In> {
    /// Construct a new `NetworkReceiver`.
    ///
    /// To get its sender use `.sender()` for a `NetworkSender` or directly `.local_sender` for the
    /// raw channel.
    pub fn new(receiver_endpoint: ReceiverEndpoint) -> Self {
        let (sender, receiver) = BoundedChannelReceiver::new(CHANNEL_CAPACITY);
        Self {
            receiver_endpoint,
            receiver,
            local_sender: Some(sender),
        }
    }

    /// Obtain a `NetworkSender` that will send messages that will arrive to this receiver.
    pub fn sender(&mut self) -> Option<NetworkSender<In>> {
        self.local_sender
            .take()
            .map(|sender| NetworkSender::local(self.receiver_endpoint, sender))
    }

    /// Receive a message from any sender.
    #[allow(dead_code)]
    pub fn recv(&self) -> Result<In> {
        self.receiver.recv().map_err(|e| {
            anyhow!(
                "Failed to receive from channel at {:?}: {:?}",
                self.receiver_endpoint,
                e
            )
        })
    }

    /// Receive a message from any sender with a timeout.
    #[allow(dead_code)]
    pub fn recv_timeout(&self, timeout: Duration) -> Result<In, RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }

    /// Receive a message from any sender of this receiver of the other provided receiver.
    ///
    /// The first message of the two is returned. If both receivers are ready one of them is chosen
    /// randomly (with an unspecified probability). It's guaranteed this function has the eventual
    /// fairness property.
    #[allow(dead_code)] // TODO: remove once joins are implemented
    pub fn select<In2: Data>(&self, other: &NetworkReceiver<In2>) -> SelectResult<In, In2> {
        self.receiver.select(&other.receiver)
    }

    /// Same as `select`, with a timeout.
    #[allow(dead_code)] // TODO: remove once joins are implemented
    pub fn select_timeout<In2: Data>(
        &self,
        other: &NetworkReceiver<In2>,
        timeout: Duration,
    ) -> Result<SelectResult<In, In2>, RecvTimeoutError> {
        self.receiver.select_timeout(&other.receiver, timeout)
    }

    /// Same as `select`, but takes multiple receivers to select from.
    pub fn select_any(receivers: &[NetworkReceiver<In>]) -> SelectAnyResult<In> {
        BoundedChannelReceiver::select_any(receivers.iter().map(|r| &r.receiver))
    }

    /// Same as `select_timeout`, but takes multiple receivers to select from.
    pub fn select_any_timeout(
        receivers: &[NetworkReceiver<In>],
        timeout: Duration,
    ) -> Result<SelectAnyResult<In>, RecvTimeoutError> {
        BoundedChannelReceiver::select_any_timeout(receivers.iter().map(|r| &r.receiver), timeout)
    }
}
