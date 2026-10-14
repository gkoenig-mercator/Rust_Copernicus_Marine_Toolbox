//! Barebone Rust port of the Copernicus Marine Toolbox.
//!
//! The library holds all the logic so that it can be reused both by the
//! `copernicusmarine` command line interface (`src/main.rs`) and, later on,
//! by Python bindings (PyO3/maturin).

pub mod catalogue;
pub mod codecs;
pub mod config;
pub mod credentials;
pub mod describe;
pub mod get;
pub mod http;
pub mod netcdf3;
pub mod subset;
pub mod utils;
pub mod zarr;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
