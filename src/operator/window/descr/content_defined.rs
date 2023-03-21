use std::time::{Duration, Instant};

use super::super::*;
use crate::operator::{Data, StreamElement};

#[derive(Clone)]
pub struct ContentWindowManager<A, F>
where
    A: WindowAccumulator,
    F: Fn(&A::In) -> Option<Timestamp>,
{
    init: A,
    f: F,
    gap: Duration,
    w: Option<Slot<A>>,
}

#[derive(Clone)]
struct Slot<A> {
    acc: A,
    close: Option<Timestamp>,
}

impl<A> Slot<A> {
    #[inline]
    fn new(acc: A) -> Self {
        Self { acc, last }
    }
}

impl<A: WindowAccumulator> WindowManager for ContentWindowManager<A>
where
    A::In: Data,
    A::Out: Data,
{
    type In = A::In;
    type Out = A::Out;
    type Output = Option<WindowResult<A::Out>>;

    #[inline]
    fn process(&mut self, el: StreamElement<A::In>) -> Self::Output {
        let ts = Instant::now();

        let ret = match &self.w {
            Some(slot) if ts - slot.last > self.gap => {
                let output = self.w.take().unwrap().acc.output();
                Some(WindowResult::Item(output))
            }
            _ => None,
        };

        match el {
            StreamElement::Item(item) | StreamElement::Timestamped(item, _) => {
                let slot = self
                    .w
                    .get_or_insert_with(|| Slot::new(self.init.clone(), ts));
                slot.acc.process(item);
                slot.last = ts;
                ret
            }
            StreamElement::Terminate | StreamElement::FlushAndRestart => {
                ret.or_else(|| self.w.take().map(|s| WindowResult::Item(s.acc.output())))
            }
            _ => ret,
        }
    }
}

#[derive(Clone)]
pub struct ContentDefinedWindow {
    gap: Duration,
}

impl ContentDefinedWindow {
    #[inline]
    pub fn new(gap_millis: Duration) -> Self {
        assert!(!gap_millis.is_zero(), "window size must be > 0");
        Self { gap: gap_millis }
    }
}

impl WindowBuilder for ContentDefinedWindow {
    type Manager<A: WindowAccumulator> = ContentWindowManager<A>;

    #[inline]
    fn build<A: WindowAccumulator>(&self, accumulator: A) -> Self::Manager<A> {
        ContentWindowManager {
            init: accumulator,
            gap: self.gap,
            w: Default::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::operator::window::aggr::Fold;

    macro_rules! save_result {
        ($ret:expr, $v:expr) => {{
            let iter = $ret.into_iter().map(|r| r.unwrap_item());
            $v.extend(iter);
        }};
    }

    #[test]
    fn event_time_window() {
        let window = ContentDefinedWindow::new(Duration::from_millis(10));

        let fold = Fold::new(Vec::new(), |v, el| v.push(el));
        let mut manager = window.build(fold);

        let mut received = Vec::new();
        for i in 0..100i64 {
            if i == 33 || i == 80 {
                std::thread::sleep(Duration::from_millis(11))
            }
            save_result!(
                manager.process(StreamElement::Timestamped(i, i / 10)),
                received
            );
        }
        save_result!(manager.process(StreamElement::FlushAndRestart), received);

        received.sort();

        let expected: Vec<Vec<_>> =
            vec![(0..33).collect(), (33..80).collect(), (80..100).collect()];
        assert_eq!(received, expected)
    }
}