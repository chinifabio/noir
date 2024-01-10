/*!
# Noir

[Preprint](https://arxiv.org/abs/2306.04421)

### Network of Operators In Rust

[API Docs](https://deib-polimi.github.io/noir/noir/)

Noir is a distributed data processing platform based on the dataflow paradigm that provides an ergonomic programming interface, similar to that of Apache Flink, but has much better performance characteristics.


Noir converts each job into a dataflow graph of
operators and groups them in blocks. Blocks contain a sequence of operors which process the data sequentially without repartitioning it. They are the deployment unit used by the system and can be distributed and executed on multiple systems.

The common layout of a Noir program starts with the creation of a `StreamEnvironment`, then one or more `Source`s are initialised creating a `Stream`. The graph of operators is composed using the methods of the `Stream` object, which follow a similar approach to Rust's `Iterator` trait allowing ergonomically define a processing workflow through method chaining.

### Examples

#### Wordcount

```no_run
use noir_compute::prelude::*;

fn main() {
    // Convenience method to parse deployment config from CLI arguments
    let (config, args) = EnvironmentConfig::from_args();
    let mut env = StreamEnvironment::new(config);
    env.spawn_remote_workers();

    let result = env
        // Open and read file line by line in parallel
        .stream_file(&args[0])
        // Split into words
        .flat_map(|line| tokenize(&line))
        // Partition
        .group_by(|word| word.clone())
        // Count occurrences
        .fold(0, |count, _word| *count += 1)
        // Collect result
        .collect_vec();

    env.execute_blocking(); // Start execution (blocking)
    if let Some(result) = result.get() {
        // Print word counts
        result.into_iter().for_each(|(word, count)| println!("{word}: {count}"));
    }
}

fn tokenize(s: &str) -> Vec<String> {
    // Simple tokenisation strategy
    s.split_whitespace().map(str::to_lowercase).collect()
}

// Execute on 6 local hosts `cargo run -- -l 6 input.txt`
```

#### Wordcount associative (faster)


```no_run
use noir_compute::prelude::*;

fn main() {
    // Convenience method to parse deployment config from CLI arguments
    let (config, args) = EnvironmentConfig::from_args();
    let mut env = StreamEnvironment::new(config);

    let result = env
        .stream_file(&args[0])
        // Adaptive batching(default) has predictable latency
        // Fixed size batching often leads to shorter execution times
        // If data is immediately available and latency is not critical
        .batch_mode(BatchMode::fixed(1024))
        .flat_map(move |line| tokenize(&line))
        .map(|word| (word, 1))
        // Associative operators split the operation in a local and a
        // global step for faster execution
        .group_by_reduce(|w| w.clone(), |(_w1, c1), (_w2, c2)| *c1 += c2)
        .drop_key()
        .collect_vec();

    env.execute_blocking(); // Start execution (blocking)
    if let Some(result) = result.get() {
        // Print word counts
        result.into_iter().for_each(|(word, count)| println!("{word}: {count}"));
    }
}

fn tokenize(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_lowercase).collect()
}

// Execute on multiple hosts `cargo run -- -r config.yaml input.txt`
```

### Remote deployment

```yaml
# config.yaml
hosts:
  - address: host1.lan
    base_port: 9500
    num_cores: 16
  - address: host2.lan
    base_port: 9500
    num_cores: 8
    ssh:
      username: noir-compute
      key_file: /home/user/.ssh/id_rsa
```

Refer to the [examples](examples/) directory for an extended set of working examples
*/
#[macro_use]
extern crate derivative;
#[macro_use]
extern crate log;

use std::ops::{Add, AddAssign};

use serde::{Deserialize, Serialize};

pub use block::structure;
pub use block::BatchMode;
pub use block::Replication;
pub use config::EnvironmentConfig;
pub use environment::StreamEnvironment;
pub use operator::iteration::IterationStateHandle;
pub use scheduler::ExecutionMetadata;
pub use stream::{KeyedStream, Stream, WindowedStream};

use crate::block::BlockStructure;
use crate::network::Coord;
use crate::profiler::ProfilerResult;

pub(crate) mod block;
pub mod box_op;
pub(crate) mod channel;
pub mod config;
pub mod data_type;
pub(crate) mod environment;
pub(crate) mod network;
pub mod operator;
pub mod optimization;
mod profiler;
#[cfg(feature = "ssh")]
pub(crate) mod runner;
pub(crate) mod scheduler;
pub(crate) mod stream;
#[cfg(test)]
pub(crate) mod test;
pub(crate) mod worker;

pub type CoordUInt = u64;

/// Re-export of commonly used structs and traits
pub mod prelude {
    pub use super::operator::sink::StreamOutput;
    pub use super::operator::source::*;
    pub use super::operator::window::{CountWindow, ProcessingTimeWindow, SessionWindow};
    #[cfg(feature = "timestamp")]
    pub use super::operator::window::{EventTimeWindow, TransactionWindow};
    pub use super::{BatchMode, EnvironmentConfig, StreamEnvironment};
}

/// Tracing information of the current execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct TracingData {
    structures: Vec<(Coord, BlockStructure)>,
    profilers: Vec<ProfilerResult>,
}

impl Add for TracingData {
    type Output = TracingData;

    fn add(mut self, rhs: Self) -> Self::Output {
        self += rhs;
        self
    }
}

impl AddAssign for TracingData {
    fn add_assign(&mut self, mut rhs: Self) {
        self.structures.append(&mut rhs.structures);
        self.profilers.append(&mut rhs.profilers);
    }
}
