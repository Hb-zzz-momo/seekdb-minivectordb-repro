//! Durable storage. The append-only commit log in `log` is the only copy of
//! table data; `catalog` and `row` define what goes into it.

pub mod catalog;
pub mod codec;
pub mod crc;
pub mod log;
pub mod row;
