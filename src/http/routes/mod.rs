//! HTTP route handlers — each route in its own submodule.

pub mod balance;
pub mod discovery;
#[cfg(test)]
mod discovery_test;
pub mod ln_invoice;
pub mod pay;
#[cfg(test)]
mod pay_session_test;
pub mod usage;
pub mod whoami;
