//! Background tasks of the gear (spec §13): background indexing, the orphan
//! turn watchdog and the upload reaper (the last two under a leader elector).

pub mod background_indexing;
pub mod leader;
pub mod orphan_watchdog;
pub mod upload_reaper;
