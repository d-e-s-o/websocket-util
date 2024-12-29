// Copyright (C) 2019-2024 Daniel Mueller <deso@posteo.net>
// SPDX-License-Identifier: GPL-3.0-or-later

#![warn(
  bad_style,
  dead_code,
  future_incompatible,
  improper_ctypes,
  late_bound_lifetime_arguments,
  missing_copy_implementations,
  missing_debug_implementations,
  missing_docs,
  no_mangle_generic_items,
  non_shorthand_field_patterns,
  nonstandard_style,
  overflowing_literals,
  path_statements,
  patterns_in_fns_without_body,
  proc_macro_derive_resolution_fallback,
  renamed_and_removed_lints,
  rust_2018_compatibility,
  rust_2018_idioms,
  stable_features,
  trivial_bounds,
  trivial_numeric_casts,
  type_alias_bounds,
  tyvar_behind_raw_pointer,
  unconditional_recursion,
  unreachable_code,
  unreachable_patterns,
  unstable_features,
  unstable_name_collisions,
  unused,
  unused_comparisons,
  unused_import_braces,
  unused_lifetimes,
  unused_qualifications,
  unused_results,
  while_true,
  rustdoc::broken_intra_doc_links
)]
#![allow(clippy::let_unit_value)]

//! A crate for providing utility functionality on top of a WebSocket
//! channel.

/// Logic for associating a subscription-style controller object with a
/// WebSocket stream.
pub mod subscribe;
/// Functionality for wrapping a WebSocket stream, adding automated
/// support for handling of control messages.
pub mod wrap;

/// Re-export of the tungstenite version the crate interfaces with.
pub use tokio_tungstenite::tungstenite;

/// A module providing functionality for testing WebSocket streams.
#[cfg(any(test, feature = "test"))]
pub mod test;
