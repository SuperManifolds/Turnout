/// Nimby Rails .nrclip data types with version-aware serialization.

use anyhow::Result;
use crate::wire::{PayloadReader, PayloadWriter};

// ══════════════════════════════════════════════════════════════════════
// Serialization traits
// ══════════════════════════════════════════════════════════════════════

/// Deserialize from the game's binary wire format.
pub trait NrclipRead: Sized {
    fn nrclip_read(r: &mut PayloadReader, ver: u32) -> Result<Self>;
}

/// Serialize to the game's binary wire format.
pub trait NrclipWrite {
    fn nrclip_write(&self, w: &mut PayloadWriter, ver: u32);
}

// ══════════════════════════════════════════════════════════════════════
// Type modules
// ══════════════════════════════════════════════════════════════════════

mod collection;
mod track;
mod signal;
mod building;
mod station;
mod kinds;
mod demand;
mod mod_meta;

// ══════════════════════════════════════════════════════════════════════
// Re-exports
// ══════════════════════════════════════════════════════════════════════

pub use collection::{Collection, Clip};
pub use track::{Track, Conflict};
pub use signal::Signal;
pub use building::{Building, BuildingPoi};
pub use station::StationGroup;
pub use kinds::{TrackKind, TrackKindHorizon, TrackTexture, BuildingKind};
pub use demand::{Demand, DemandRange};
pub use mod_meta::{ModMeta, ModRelFile};
