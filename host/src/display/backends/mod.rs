//! The desktop-specific [`LayoutBackend`](super::backend::LayoutBackend)
//! adapters. Each translates between its tool's own types and the neutral
//! [`Layout`](super::model::Layout); none of them holds policy.

#[cfg(target_os = "linux")]
pub mod gnome;
#[cfg(target_os = "linux")]
pub mod kde;
#[cfg(target_os = "linux")]
pub mod x11;
