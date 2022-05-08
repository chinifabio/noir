//! Operators that can be applied to a stream.
//!
//! The actual operator list can be found from the implemented methods of [`Stream`](crate::Stream),
//! [`KeyedStream`](crate::KeyedStream), [`WindowedStream`](crate::WindowedStream) and
//! [`KeyedWindowedStream`](crate::KeyedWindowedStream).

use std::hash::Hash;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub(crate) use start::*;

use crate::block::BlockStructure;
use crate::scheduler::ExecutionMetadata;
use crate::stream::KeyValue;

pub(crate) mod add_timestamps;
pub(crate) mod aggregators;
pub(crate) mod batch_mode;
pub(crate) mod broadcast;
pub(crate) mod concat;
pub(crate) mod end;
pub(crate) mod filter;
pub(crate) mod filter_map;
pub(crate) mod flatten;
pub(crate) mod fold;
pub(crate) mod group_by;
pub(crate) mod interval_join;
pub(crate) mod iteration;
pub mod join;
pub(crate) mod key_by;
pub(crate) mod keyed_fold;
pub(crate) mod keyed_reduce;
pub(crate) mod map;
pub(crate) mod max_parallelism;
pub(crate) mod reduce;
pub(crate) mod reorder;
pub(crate) mod rich_filter_map;
pub(crate) mod rich_flat_map;
pub(crate) mod rich_map;
pub(crate) mod shuffle;
pub mod sink;
pub mod source;
pub(crate) mod split;
pub(crate) mod start;
pub(crate) mod unkey;
pub mod window;
pub(crate) mod zip;

/// Marker trait that all the types inside a stream should implement.
pub trait Data: Clone + Send + 'static {}
impl<T: Clone + Send + 'static> Data for T {}

/// Marker trait for data types that are used to communicate between different blocks.
pub trait ExchangeData: Data + Serialize + for<'a> Deserialize<'a> {}
impl<T: Data + Serialize + for<'a> Deserialize<'a> + 'static> ExchangeData for T {}

/// Marker trait that all the keys should implement.
pub trait DataKey: Data + Hash + Eq {}
impl<T: Data + Hash + Eq> DataKey for T {}

/// Marker trait for key types that are used when communicating between different blocks.
pub trait ExchangeDataKey: DataKey + ExchangeData {}
impl<T: DataKey + ExchangeData> ExchangeDataKey for T {}

/// Marker trait for the function that extracts the key out of a type.
pub trait KeyerFn<Key, Out>: Fn(&Out) -> Key + Clone + Send + 'static {}
impl<Key, Out, T: Fn(&Out) -> Key + Clone + Send + 'static> KeyerFn<Key, Out> for T {}

/// When using timestamps and watermarks, this type expresses the timestamp of a message or of a
/// watermark.
pub type Timestamp = Duration;
/// Returns `Duration::new(u64::MAX, 1_000_000_000 - 1)`, which is equivalent to `Duration::MAX`.
/// This is needed because `Duration::MAX` is unstable and `Duration::new` cannot be used to
/// initialize a constant value.
pub(crate) fn timestamp_max() -> Duration {
    Duration::new(u64::MAX, 1_000_000_000 - 1)
}

/// An element of the stream. This is what enters and exits from the operators.
///
/// An operator may need to change the content of a `StreamElement` (e.g. a `Map` may change the
/// value of the `Item`). Usually `Watermark` and `FlushAndRestart` are simply forwarded to the next
/// operator in the chain.
///
/// In general a stream may be composed of a sequence of this kind:
///
/// `((Item | Timestamped | Watermark | FlushBatch)* FlushAndRestart)+ Terminate`
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Ord, PartialOrd)]
pub enum StreamElement<Out> {
    /// A normal element containing just the value of the message.
    Item(Out),
    /// Like `Item`, but it's attached with a timestamp, it's used to ensure the ordering of the
    /// messages.
    Timestamped(Out, Timestamp),
    /// When an operator receives a `Watermark` with timestamp `t`, the operator will never see any
    /// message with timestamp less or equal to `t`.
    Watermark(Timestamp),
    /// Flush the internal batch since there will be too much delay till the next message to come.
    FlushBatch,
    /// The stream has ended, and the operators should exit as soon as possible.
    ///
    /// No messages should be generated by the operator between a `FlushAndRestart` and a
    /// `Terminate`.
    Terminate,
    /// Mark the end of a stream of data.
    ///
    /// Note that this does not mean that the entire stream has ended, for example this is used to
    /// mark the end of an iteration. Therefore an operator may be prepared to received new data
    /// after this message, but should not retain the internal state.
    FlushAndRestart,
}

/// An operator represents a unit of computation. It's always included inside a chain of operators,
/// inside a block.
///
/// Each operator implements the `Operator<Out>` trait, it produced a stream of `Out` elements.
///
/// An `Operator` must be Clone since it is part of a single chain when it's built, but it has to
/// be cloned to spawn the replicas of the block.
pub trait Operator<Out: Data>: Clone + Send {
    /// Setup the operator chain. This is called before any call to `next` and it's used to
    /// initialize the operator. When it's called the operator has already been cloned and it will
    /// never be cloned again. Therefore it's safe to store replica-specific metadata inside of it.
    ///
    /// It's important that each operator (except the start of a chain) calls `.setup()` recursively
    /// on the previous operators.
    fn setup(&mut self, metadata: &mut ExecutionMetadata);

    /// Take a value from the previous operator, process it and return it.
    fn next(&mut self) -> StreamElement<Out>;

    /// A string representation of the operator and its predecessors.
    fn to_string(&self) -> String;

    /// A more refined representation of the operator and its predecessors.
    fn structure(&self) -> BlockStructure;
}

impl<Out: Data> StreamElement<Out> {
    /// Create a new `StreamElement` with an `Item(())` if `self` contains an item, otherwise it
    /// returns the same variant of `self`.
    pub(crate) fn take(&self) -> StreamElement<()> {
        match self {
            StreamElement::Item(_) => StreamElement::Item(()),
            StreamElement::Timestamped(_, _) => StreamElement::Item(()),
            StreamElement::Watermark(w) => StreamElement::Watermark(*w),
            StreamElement::Terminate => StreamElement::Terminate,
            StreamElement::FlushAndRestart => StreamElement::FlushAndRestart,
            StreamElement::FlushBatch => StreamElement::FlushBatch,
        }
    }

    /// Change the type of the element inside the `StreamElement`.
    pub(crate) fn map<NewOut: Data>(self, f: impl FnOnce(Out) -> NewOut) -> StreamElement<NewOut> {
        match self {
            StreamElement::Item(item) => StreamElement::Item(f(item)),
            StreamElement::Timestamped(item, ts) => StreamElement::Timestamped(f(item), ts),
            StreamElement::Watermark(w) => StreamElement::Watermark(w),
            StreamElement::Terminate => StreamElement::Terminate,
            StreamElement::FlushAndRestart => StreamElement::FlushAndRestart,
            StreamElement::FlushBatch => StreamElement::FlushBatch,
        }
    }

    /// A string representation of the variant of this `StreamElement`.
    pub(crate) fn variant(&self) -> &'static str {
        match self {
            StreamElement::Item(_) => "Item",
            StreamElement::Timestamped(_, _) => "Timestamped",
            StreamElement::Watermark(_) => "Watermark",
            StreamElement::FlushBatch => "FlushBatch",
            StreamElement::Terminate => "Terminate",
            StreamElement::FlushAndRestart => "FlushAndRestart",
        }
    }
}

impl<Key: DataKey, Out: Data> StreamElement<KeyValue<Key, Out>> {
    /// Map a `StreamElement<KeyValue(Key, Out)>` to a `StreamElement<Out>`,
    /// returning the key if possible
    pub(crate) fn remove_key(self) -> (Option<Key>, StreamElement<Out>) {
        match self {
            StreamElement::Item((k, v)) => (Some(k), StreamElement::Item(v)),
            StreamElement::Timestamped((k, v), ts) => (Some(k), StreamElement::Timestamped(v, ts)),
            _ => (None, self.map(|_| unreachable!())),
        }
    }
}
