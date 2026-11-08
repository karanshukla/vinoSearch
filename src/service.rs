//! The long-running runner: the D-Bus interfaces, the worker thread that owns
//! the model and the index, the file watcher, and when indexing may run.

pub mod krunner;
pub mod power;
pub mod sched;
pub mod watch;
pub mod worker;
