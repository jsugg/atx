//! Stand-ins for the crate-root domain re-exports used by included modules.

#[path = "../../src/domain/id.rs"]
pub(crate) mod id_shim;
#[path = "../../src/domain/primitives.rs"]
pub(crate) mod primitives_shim;

pub(crate) use id_shim::JobId;
pub(crate) use primitives_shim::Revision;
