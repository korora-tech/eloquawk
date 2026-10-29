//! Mail domain logic that needs no network or UI: base64, payload parsing, HTML to text,
//! link detection, `mailto:` parsing, addresses and local query matching.

pub mod address;
pub mod b64;
pub mod html;
pub mod links;
pub mod mailto;
pub mod parse;
pub mod query;
