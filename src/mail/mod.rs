//! Mail domain logic that needs no network or UI: base64, payload parsing, HTML to text,
//! link detection, `mailto:` parsing, addresses, local query matching, and the composer's
//! reply prefills and outgoing MIME.

pub mod address;
pub mod b64;
pub mod compose;
pub mod html;
pub mod links;
pub mod mailto;
pub mod parse;
pub mod query;
pub mod reply;
