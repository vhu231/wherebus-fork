#[cfg(feature = "bot")]
pub mod bot;
pub mod web;
pub(crate) mod domain;
pub(crate) mod provider;
pub(crate) mod support;

pub(crate) use domain as models;
pub(crate) use provider as providers;
