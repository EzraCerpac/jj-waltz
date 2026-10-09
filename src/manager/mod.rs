//! Interactive workspace management. Terminal state stays separate from repository services.
pub(crate) mod preferences;
pub(crate) mod removal;
pub(crate) mod status;
mod ui;

pub(crate) use ui::run;
