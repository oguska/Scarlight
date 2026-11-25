//! Reading the console's title formats: container formats, build layouts,
//! the identities we print for a build.
//!
//! What belongs here is our own code that understands a title's files. What
//! one build needs from this layer — code hooks, shaders to skip, its label —
//! is user data and lives in a profile outside the repository
//! (`victory_core::profile`, `docs/title-profiles.md`), so no title is known
//! here. What may and may not be stored is defined in `docs/asset-policy.md`.

pub mod sgd5;
