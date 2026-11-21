//! A calendar as a Yoke Plugin: the events of the sources its arguments name, read, kept and expanded.
//!
//! `sources` knows the sources and the unit's arguments, `read` reads a source and keeps what was read, `ics`
//! reads iCalendar and expands it, and `plugin` is the Plugin around them.

pub mod accounts;
pub mod ics;
pub mod online;
pub mod plugin;
pub mod read;
pub mod sources;
