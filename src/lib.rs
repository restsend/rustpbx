pub mod app;
pub mod config;
pub mod console;
pub mod error;
pub mod handler;
pub mod media;
pub mod proxy;
pub mod useragent;

pub use error::Error;
pub type Result<T> = std::result::Result<T, Error>;
