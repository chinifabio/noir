use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::block::{BlockStructure, OperatorReceiver, OperatorStructure};
use crate::channel::{RecvTimeoutError, SelectResult};
use crate::network::{Coord, NetworkMessage};
use crate::operator::start::{SimpleStartReceiver, StartReceiver};
use crate::operator::{Data, ExchangeData, StreamElement};
use crate::scheduler::{BlockId, ExecutionMetadata};

/// This enum is an _either_ type, it contain either an element from the left part or an element
/// from the right part.
///
/// Since those two parts are merged the information about which one ends is lost, therefore there
/// are two extra variants to keep track of that.
#[derive(Clone, Debug, Serialize, Deserialize, Ord, PartialOrd, Eq, PartialEq)]
pub enum BinaryElement<OutL: Data, OutR: Data> {
    /// An element of the stream on the left.
    Left(OutL),
    /// An element of the stream on the right.
    Right(OutR),
    /// The left side has ended.
    LeftEnd,
    /// The right side has ended.
    RightEnd,
}

/// The actual receiver from one of the two sides.
#[derive(Clone, Debug)]
struct SideReceiver<Out: ExchangeData, Item: ExchangeData> {
    /// The internal receiver for this side.
    receiver: SimpleStartReceiver<Out>,
    /// The number of replicas this side has.
    instances: usize,
    /// How many replicas from this side has not yet sent `StreamElement::FlushAndRestart`.
    missing_flush_and_restart: usize,
    /// How many replicas from this side has not yet sent `StreamElement::Terminate`.
    missing_terminate: usize,
    /// Whether this side is cached (i.e. after it is fully read, it's just replayed indefinitely).
    cached: bool,
    /// The content of the cache, if any.
    cache: Vec<NetworkMessage<Item>>,
    /// Whether the cache has been fully populated.
    cache_full: bool,
    /// The index of the first element to return from the cache.
    cache_pointer: usize,
}

impl<Out: ExchangeData, Item: ExchangeData> SideReceiver<Out, Item> {
    fn new(previous_block_id: BlockId, cached: bool) -> Self {
        Self {
            receiver: SimpleStartReceiver::new(previous_block_id),
            instances: 0,
            missing_flush_and_restart: 0,
            missing_terminate: 0,
            cached,
            cache: Default::default(),
            cache_full: false,
            cache_pointer: 0,
        }
    }

    fn setup(&mut self, metadata: &mut ExecutionMetadata) {
        self.receiver.setup(metadata);
        self.instances = self.receiver.prev_replicas().len();
        self.missing_flush_and_restart = self.instances;
        self.missing_terminate = self.instances;
    }

    fn recv(&mut self, timeout: Option<Duration>) -> Result<NetworkMessage<Out>, RecvTimeoutError> {
        if let Some(timeout) = timeout {
            self.receiver.recv_timeout(timeout)
        } else {
            Ok(self.receiver.recv())
        }
    }

    fn reset(&mut self) {
        self.missing_flush_and_restart = self.instances;
        if self.cached {
            self.cache_full = true;
            self.cache_pointer = 0;
        }
    }

    /// There is nothing more to read from this side for this iteration.
    fn is_ended(&self) -> bool {
        if self.cached {
            self.is_terminated()
        } else {
            self.missing_flush_and_restart == 0
        }
    }

    /// No more data can come from this side ever again.
    fn is_terminated(&self) -> bool {
        self.missing_terminate == 0
    }

    /// All the cached items have already been read.
    fn cache_finished(&self) -> bool {
        self.cache_pointer >= self.cache.len()
    }

    /// Get the next batch from the cache. This will panic if the cache has been entirely consumed.
    fn next_cached_item(&mut self) -> NetworkMessage<Item> {
        self.cache_pointer += 1;
        if self.cache_finished() {
            // Items are simply returned, so flush and restarts are not counted properly. Just make
            // sure that when the cache ends the counter is zero.
            self.missing_flush_and_restart = 0;
        }
        self.cache[self.cache_pointer - 1].clone()
    }
}

/// This receiver is able to receive data from two previous blocks.
///
/// To do so it will first select on the two channels, and wrap each element into an enumeration
/// that discriminates the two sides.
#[derive(Clone, Debug)]
pub struct BinaryStartReceiver<OutL: ExchangeData, OutR: ExchangeData> {
    left: SideReceiver<OutL, BinaryElement<OutL, OutR>>,
    right: SideReceiver<OutR, BinaryElement<OutL, OutR>>,
    first_message: bool,
}

impl<OutL: ExchangeData, OutR: ExchangeData> BinaryStartReceiver<OutL, OutR> {
    pub(super) fn new(
        left_block_id: BlockId,
        right_block_id: BlockId,
        left_cache: bool,
        right_cache: bool,
    ) -> Self {
        assert!(
            !(left_cache && right_cache),
            "At most one of the two sides can be cached"
        );
        Self {
            left: SideReceiver::new(left_block_id, left_cache),
            right: SideReceiver::new(right_block_id, right_cache),
            first_message: false,
        }
    }

    /// Process the incoming batch from one of the two sides.
    ///
    /// This will map all the elements of the batch into a new batch whose elements are wrapped in
    /// the variant of the correct side. Additionally this will search for
    /// `StreamElement::FlushAndRestart` messages and eventually emit the `LeftEnd`/`RightEnd`
    /// accordingly.
    fn process_side<Out: ExchangeData>(
        side: &mut SideReceiver<Out, BinaryElement<OutL, OutR>>,
        message: NetworkMessage<Out>,
        wrap: fn(Out) -> BinaryElement<OutL, OutR>,
        end: BinaryElement<OutL, OutR>,
    ) -> NetworkMessage<BinaryElement<OutL, OutR>> {
        let sender = message.sender();
        let data = message
            .into_iter()
            .flat_map(|item| {
                let mut res = Vec::new();
                if matches!(item, StreamElement::FlushAndRestart) {
                    side.missing_flush_and_restart -= 1;
                    // make sure to add this message before `FlushAndRestart`
                    if side.missing_flush_and_restart == 0 {
                        res.push(StreamElement::Item(end.clone()));
                    }
                }
                if matches!(item, StreamElement::Terminate) {
                    side.missing_terminate -= 1;
                }
                // StreamElement::Terminate should not be put in the cache
                if !side.cached || !matches!(item, StreamElement::Terminate) {
                    res.push(item.map(wrap));
                }
                res
            })
            .collect::<Vec<_>>();
        let message = NetworkMessage::new_batch(data, sender);
        if side.cached {
            side.cache.push(message.clone());
            // the elements are already out, ignore the cache for this round
            side.cache_pointer = side.cache.len();
        }
        message
    }

    /// Receive from the previous sides the next batch, or fail with a timeout if provided.
    ///
    /// This will access only the needed side (i.e. if one of the sides ended, only the other is
    /// probed). This will try to use the cache if it's available.
    fn select(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<NetworkMessage<BinaryElement<OutL, OutR>>, RecvTimeoutError> {
        // both sides received all the `StreamElement::Terminate`, but the cached ones have never
        // been emitted
        if self.left.is_terminated() && self.right.is_terminated() {
            let num_terminates = if self.left.cached {
                self.left.instances
            } else if self.right.cached {
                self.right.instances
            } else {
                0
            };
            if num_terminates > 0 {
                return Ok(NetworkMessage::new_batch(
                    (0..num_terminates)
                        .map(|_| StreamElement::Terminate)
                        .collect(),
                    Default::default(),
                ));
            }
        }

        // both left and right received all the FlushAndRestart, prepare for the next iteration
        if self.left.is_ended()
            && self.right.is_ended()
            && self.left.cache_finished()
            && self.right.cache_finished()
        {
            self.left.reset();
            self.right.reset();
            self.first_message = true;
        }

        enum Side<L, R> {
            Left(L),
            Right(R),
        }

        // First message of this iteration, and there is a side with the cache:
        // we need to ask to the other side FIRST to know if this is the end of the stream or a new
        // iteration is about to start.
        let data = if self.first_message && (self.left.cached || self.right.cached) {
            debug_assert!(!self.left.cached || self.left.cache_full);
            debug_assert!(!self.right.cached || self.right.cache_full);
            self.first_message = false;
            if self.left.cached {
                Side::Right(self.right.recv(timeout))
            } else {
                Side::Left(self.left.recv(timeout))
            }
        } else if self.left.cached && self.left.cache_full && !self.left.cache_finished() {
            // The left side is cached, therefore we can access it immediately
            return Ok(self.left.next_cached_item());
        } else if self.right.cached && self.right.cache_full && !self.right.cache_finished() {
            // The right side is cached, therefore we can access it immediately
            return Ok(self.right.next_cached_item());
        } else if self.left.is_ended() {
            // There is nothing more to read from the left side (if cached, all the cache has
            // already been read).
            Side::Right(self.right.recv(timeout))
        } else if self.right.is_ended() {
            // There is nothing more to read from the right side (if cached, all the cache has
            // already been read).
            Side::Left(self.left.recv(timeout))
        } else {
            let left_terminated = self.left.is_terminated();
            let right_terminated = self.right.is_terminated();
            let left = self.left.receiver.receiver.as_mut().unwrap();
            let right = self.right.receiver.receiver.as_mut().unwrap();

            let data = match (left_terminated, right_terminated, timeout) {
                (false, false, Some(timeout)) => left.select_timeout(right, timeout),
                (false, false, None) => Ok(left.select(right)),

                (true, false, Some(timeout)) => {
                    right.recv_timeout(timeout).map(|r| SelectResult::B(Ok(r)))
                }
                (false, true, Some(timeout)) => {
                    left.recv_timeout(timeout).map(|r| SelectResult::A(Ok(r)))
                }

                (true, false, None) => Ok(SelectResult::B(right.recv())),
                (false, true, None) => Ok(SelectResult::A(left.recv())),

                (true, true, _) => Err(RecvTimeoutError::Disconnected),
            };

            match data {
                Ok(SelectResult::A(left)) => {
                    Side::Left(left.map_err(|_| RecvTimeoutError::Disconnected))
                }
                Ok(SelectResult::B(right)) => {
                    Side::Right(right.map_err(|_| RecvTimeoutError::Disconnected))
                }
                // timeout
                Err(e) => Side::Left(Err(e)),
            }
        };

        match data {
            Side::Left(Ok(left)) => Ok(Self::process_side(
                &mut self.left,
                left,
                BinaryElement::Left,
                BinaryElement::LeftEnd,
            )),
            Side::Right(Ok(right)) => Ok(Self::process_side(
                &mut self.right,
                right,
                BinaryElement::Right,
                BinaryElement::RightEnd,
            )),
            Side::Left(Err(e)) | Side::Right(Err(e)) => Err(e),
        }
    }
}

impl<OutL: ExchangeData, OutR: ExchangeData> StartReceiver for BinaryStartReceiver<OutL, OutR> {
    type Out = BinaryElement<OutL, OutR>;

    fn setup(&mut self, metadata: &mut ExecutionMetadata) {
        self.left.setup(metadata);
        self.right.setup(metadata);
    }

    fn prev_replicas(&self) -> Vec<Coord> {
        let mut previous = self.left.receiver.prev_replicas();
        previous.append(&mut self.right.receiver.prev_replicas());
        previous
    }

    fn cached_replicas(&self) -> usize {
        let mut cached = 0;
        if self.left.cached {
            cached += self.left.instances
        }
        if self.right.cached {
            cached += self.right.instances
        }
        cached
    }

    fn recv_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<NetworkMessage<BinaryElement<OutL, OutR>>, RecvTimeoutError> {
        self.select(Some(timeout))
    }

    fn recv(&mut self) -> NetworkMessage<BinaryElement<OutL, OutR>> {
        self.select(None).expect("receiver failed")
    }

    fn structure(&self) -> BlockStructure {
        let mut operator = OperatorStructure::new::<BinaryElement<OutL, OutR>, _>("Start");
        operator.receivers.push(OperatorReceiver::new::<OutL>(
            self.left.receiver.previous_block_id,
        ));
        operator.receivers.push(OperatorReceiver::new::<OutR>(
            self.right.receiver.previous_block_id,
        ));

        BlockStructure::default().add_operator(operator)
    }
}
