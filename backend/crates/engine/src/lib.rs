//! Tip use cases: turning GitHub events into tips, verifying them, paying them and replying in the
//! thread. Shared by the `api` (which enqueues work) and the `worker` (which runs it).

pub mod jobs;
