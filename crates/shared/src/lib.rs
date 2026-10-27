//! What vinoSearch's daemon, CLI and settings app agree on: the config file
//! and the names on the session bus. Depends on nothing heavier than serde,
//! so the settings app can use it without pulling in OpenVINO.

pub mod apps;
pub mod config;
pub mod dbus;
pub mod limits;
