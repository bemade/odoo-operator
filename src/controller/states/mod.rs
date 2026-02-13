//! State implementations for each OdooInstancePhase.
//!
//! Each state is a zero-sized struct that implements [`State`].  The
//! `state_for()` function maps a runtime phase value to a `&'static dyn State`.

use async_trait::async_trait;

use crate::crd::odoo_instance::{OdooInstance, OdooInstancePhase};
use crate::error::Result;

use super::odoo_instance::Context;
use super::state_machine::ReconcileSnapshot;

mod backing_up;
mod degraded;
mod error;
mod init_failed;
mod initializing;
mod provisioning;
mod restoring;
mod running;
mod starting;
mod stopped;
mod uninitialized;
mod upgrading;

pub use backing_up::BackingUp;
pub use degraded::Degraded;
pub use error::Error;
pub use init_failed::InitFailed;
pub use initializing::Initializing;
pub use provisioning::Provisioning;
pub use restoring::Restoring;
pub use running::Running;
pub use starting::Starting;
pub use stopped::Stopped;
pub use uninitialized::Uninitialized;
pub use upgrading::Upgrading;

/// Idempotent outputs for a phase — called every reconcile tick while the
/// instance is in this state.  Examines the snapshot and corrects any drift.
/// Must be safe to call repeatedly (PLC-style: "in this state, these outputs
/// are energised").
#[async_trait]
pub trait State: Send + Sync {
    async fn ensure(
        &self,
        instance: &OdooInstance,
        ctx: &Context,
        snapshot: &ReconcileSnapshot,
    ) -> Result<()>;
}

/// Map a runtime phase value to a static state implementation.
pub fn state_for(phase: &OdooInstancePhase) -> &'static dyn State {
    match phase {
        OdooInstancePhase::Provisioning => &Provisioning,
        OdooInstancePhase::Uninitialized => &Uninitialized,
        OdooInstancePhase::Initializing => &Initializing,
        OdooInstancePhase::InitFailed => &InitFailed,
        OdooInstancePhase::Starting => &Starting,
        OdooInstancePhase::Running => &Running,
        OdooInstancePhase::Degraded => &Degraded,
        OdooInstancePhase::Stopped => &Stopped,
        OdooInstancePhase::Upgrading => &Upgrading,
        OdooInstancePhase::Restoring => &Restoring,
        OdooInstancePhase::BackingUp => &BackingUp,
        OdooInstancePhase::Error => &Error,
    }
}
