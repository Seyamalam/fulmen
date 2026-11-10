//! Fulmen server: the tokio + hyper HTTP/1.1 and HTTP/2 server that owns the
//! socket and runs declared middleware before any Python code is called.

/// Component name, reported by `fulmen._fulmen.native_components()`.
pub const NAME: &str = "server";
