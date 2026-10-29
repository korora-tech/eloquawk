//! Google layer: OAuth sign-in and tokens (`oauth`), the HTTP helpers and error
//! classification (`api`), and the Gmail DTOs and endpoints (`gmail`).

pub mod api;
#[cfg(test)]
pub mod fake;
pub mod gmail;
pub mod oauth;
