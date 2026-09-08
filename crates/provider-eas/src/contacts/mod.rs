// SPDX-License-Identifier: MPL-2.0
//! MS-ASCNTC Contacts-class item model (the Contact Class protocol the
//! M8-C plan calls MS-ASAIRCONT; `docs/Exchange/[MS-ASCONT].pdf`) +
//! downsync parse of a Contacts-class `ApplicationData` element, the
//! neutral conversion (`convert`), and the upsync write seam (`write`).
//!
//! Token fidelity red line: every token value lives in the `tokens`
//! submodule (split out for the 500-line rule, re-exported below so
//! `contacts::CON_*` paths stay stable) with its [MS-ASWBXML] /
//! [MS-ASCNTC] citations — never from memory.
//!
//! Parse policy (the Email `parse_application_data` precedent in the
//! `commands/sync/` module, `parse_item.rs`, and the `calendar/` twin):
//! malformed values → `log::warn!` with the element name + offending text,
//! then default — never panic, never swallow silently; tokens this task
//! does not model → `log::debug!` skip.

mod convert;
mod emit;
mod model;
mod parse;
#[cfg(test)]
pub(crate) mod tests;
mod tokens;
mod write;
mod write_patch;

pub(crate) use convert::contact_card_from_props;
pub use emit::build_contacts_application_data;
pub use model::{ContactsAddress, ContactsContactProps};
pub use parse::parse_contacts_application_data;
pub use tokens::*;
pub(crate) use write::write_from_draft;
pub(crate) use write_patch::write_from_patch;
