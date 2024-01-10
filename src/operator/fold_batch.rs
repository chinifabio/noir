use std::fmt::Display;
use std::marker::PhantomData;

use crate::block::{BlockStructure, OperatorStructure};
use crate::operator::{Data, Operator, StreamElement, Timestamp};
use crate::scheduler::ExecutionMetadata;

#[derive(Clone, Derivative)]
#[derivative(Debug)]
pub struct FoldBatch<Out: Data, NewOut: Data, F, PreviousOperators>
where
    F: Fn(&mut NewOut, Vec<Out>) + Send + Clone,
    PreviousOperators: Operator<Out = Out>,
{
    prev: PreviousOperators,
    #[derivative(Debug = "ignore")]
    fold: F,
    init: NewOut,
    accumulator: Option<NewOut>,
    timestamp: Option<Timestamp>,
    max_watermark: Option<Timestamp>,
    received_end: bool,
    received_end_iter: bool,
    batch_size: usize,
    store: Option<Vec<Out>>,
    _out: PhantomData<Out>,
}

impl<Out: Data, NewOut: Data, F, PreviousOperators> Display
    for FoldBatch<Out, NewOut, F, PreviousOperators>
where
    F: Fn(&mut NewOut, Vec<Out>) + Send + Clone,
    PreviousOperators: Operator<Out = Out>,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} -> FoldBatch<{} -> {}>",
            self.prev,
            std::any::type_name::<Out>(),
            std::any::type_name::<NewOut>()
        )
    }
}

impl<Out: Data, NewOut: Data, F, PreviousOperators: Operator<Out = Out>>
    FoldBatch<Out, NewOut, F, PreviousOperators>
where
    F: Fn(&mut NewOut, Vec<Out>) + Send + Clone,
{
    pub(super) fn new(prev: PreviousOperators, init: NewOut, fold: F, batch_size: usize) -> Self {
        FoldBatch {
            prev,
            fold,
            init,
            accumulator: None,
            timestamp: None,
            max_watermark: None,
            received_end: false,
            received_end_iter: false,
            batch_size,
            store: None,
            _out: Default::default(),
        }
    }
}

impl<Out: Data, NewOut: Data, F, PreviousOperators> Operator
    for FoldBatch<Out, NewOut, F, PreviousOperators>
where
    F: Fn(&mut NewOut, Vec<Out>) + Send + Clone,
    PreviousOperators: Operator<Out = Out>,
{
    type Out = NewOut;

    fn setup(&mut self, metadata: &mut ExecutionMetadata) {
        self.prev.setup(metadata);
    }

    #[inline]
    fn next(&mut self) -> StreamElement<NewOut> {
        while !self.received_end {
            match self.prev.next() {
                StreamElement::Terminate => self.received_end = true,
                StreamElement::FlushAndRestart => {
                    self.received_end = true;
                    self.received_end_iter = true;
                }
                StreamElement::Watermark(ts) => {
                    self.max_watermark = Some(self.max_watermark.unwrap_or(ts).max(ts))
                }
                StreamElement::Item(item) => {
                    if self.accumulator.is_none() {
                        self.accumulator = Some(self.init.clone());
                    }
                    if self.store.is_none() {
                        self.store = Some(Vec::with_capacity(self.batch_size))
                    }
                    if let Some(a) = self.store.as_mut() {
                        a.push(item);
                        if a.len() == self.batch_size {
                            (self.fold)(
                                self.accumulator.as_mut().unwrap(),
                                self.store.take().unwrap(),
                            );
                        }
                    }
                }
                StreamElement::Timestamped(item, ts) => {
                    self.timestamp = Some(self.timestamp.unwrap_or(ts).max(ts));
                    if self.accumulator.is_none() {
                        self.accumulator = Some(self.init.clone());
                    }
                    if self.store.is_none() {
                        self.store = Some(Vec::with_capacity(self.batch_size))
                    }
                    if let Some(a) = self.store.as_mut() {
                        a.push(item);
                        if a.len() == self.batch_size {
                            (self.fold)(
                                self.accumulator.as_mut().unwrap(),
                                self.store.take().unwrap(),
                            );
                        }
                    }
                }
                // this block wont sent anything until the stream ends
                StreamElement::FlushBatch => {}
            }
        }

        // If there is an accumulated value, return it
        if let Some(acc) = self.accumulator.take().as_mut() {
            if let Some(values) = self.store.take() {
                if !values.is_empty() {
                    (self.fold)(acc, values);
                }
            }

            if let Some(ts) = self.timestamp.take() {
                return StreamElement::Timestamped(acc.to_owned(), ts);
            } else {
                return StreamElement::Item(acc.to_owned());
            }
        }

        // If watermark were received, send one downstream
        if let Some(ts) = self.max_watermark.take() {
            return StreamElement::Watermark(ts);
        }

        // the end was not really the end... just the end of one iteration!
        if self.received_end_iter {
            self.received_end_iter = false;
            self.received_end = false;
            return StreamElement::FlushAndRestart;
        }

        StreamElement::Terminate
    }

    fn structure(&self) -> BlockStructure {
        self.prev
            .structure()
            .add_operator(OperatorStructure::new::<NewOut, _>("FoldBatch"))
    }
}

#[cfg(test)]
mod tests {
    use crate::operator::fold_batch::FoldBatch;
    use crate::operator::{Operator, StreamElement};
    use crate::test::FakeOperator;

    #[test]
    fn test_fold_without_timestamps() {
        let fake_operator = FakeOperator::new(0..10u8);
        let mut fold = FoldBatch::new(
            fake_operator,
            0,
            |a, b| {
                for it in b {
                    *a += it;
                }
            },
            4,
        );

        assert_eq!(fold.next(), StreamElement::Item((0..10u8).sum()));
        assert_eq!(fold.next(), StreamElement::Terminate);
    }

    #[test]
    #[allow(clippy::identity_op)]
    #[cfg(feature = "timestamp")]
    fn test_fold_timestamped() {
        let mut fake_operator = FakeOperator::empty();
        fake_operator.push(StreamElement::Timestamped(0, 1));
        fake_operator.push(StreamElement::Timestamped(1, 2));
        fake_operator.push(StreamElement::Timestamped(2, 3));
        fake_operator.push(StreamElement::Watermark(4));

        let mut fold = FoldBatch::new(
            fake_operator,
            0,
            |a, b| {
                for it in b {
                    *a += it;
                }
            },
            4,
        );

        assert_eq!(fold.next(), StreamElement::Timestamped(0 + 1 + 2, 3));
        assert_eq!(fold.next(), StreamElement::Watermark(4));
        assert_eq!(fold.next(), StreamElement::Terminate);
    }

    #[test]
    #[allow(clippy::identity_op)]
    fn test_fold_iter_end() {
        let mut fake_operator = FakeOperator::empty();
        fake_operator.push(StreamElement::Item(0));
        fake_operator.push(StreamElement::Item(1));
        fake_operator.push(StreamElement::Item(2));
        fake_operator.push(StreamElement::FlushAndRestart);
        fake_operator.push(StreamElement::Item(3));
        fake_operator.push(StreamElement::Item(4));
        fake_operator.push(StreamElement::Item(5));
        fake_operator.push(StreamElement::FlushAndRestart);

        let mut fold = FoldBatch::new(
            fake_operator,
            0,
            |a, b| {
                for it in b {
                    *a += it;
                }
            },
            4,
        );

        assert_eq!(fold.next(), StreamElement::Item(0 + 1 + 2));
        assert_eq!(fold.next(), StreamElement::FlushAndRestart);
        assert_eq!(fold.next(), StreamElement::Item(3 + 4 + 5));
        assert_eq!(fold.next(), StreamElement::FlushAndRestart);
        assert_eq!(fold.next(), StreamElement::Terminate);
    }
}
