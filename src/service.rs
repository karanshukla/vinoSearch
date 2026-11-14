//! The long-running runner: the D-Bus interfaces, the worker thread that owns
//! the model and the index, the file watcher, and when indexing may run.

pub mod flatpak;
pub mod flatpak_update;
pub mod krunner;
pub mod power;
pub mod sched;
pub mod start;
pub mod watch;
pub mod worker;
