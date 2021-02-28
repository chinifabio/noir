use async_std::task::JoinHandle;

use crate::environment::StreamEnvironmentInner;
use crate::worker::ExecutionMetadata;

pub async fn start(env: &mut StreamEnvironmentInner) -> Vec<JoinHandle<()>> {
    let mut join = Vec::new();
    // start the execution
    for (id, handle) in env.start_handles.drain() {
        // TODO: build metadata
        let metadata = ExecutionMetadata {};
        handle.starter.send(metadata).await.unwrap();
        join.push(handle.join_handle);
    }
    join
}