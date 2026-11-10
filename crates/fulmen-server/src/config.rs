//! Server configuration: runtime shape, request limits and timeouts.

use std::io;
use std::net::SocketAddr;
use std::thread::available_parallelism;
use std::time::Duration;

/// Configuration for [`serve`](crate::serve).
///
/// All limits and timeouts default to the values in `docs/DESIGN.md` §14.3
/// and §14.4. Use `..ServerConfig::default()` to override only the fields
/// you need.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Address to bind. Use port `0` for an ephemeral port and read the
    /// bound address back with [`Server::local_addr`](crate::Server::local_addr).
    pub bind: SocketAddr,
    /// Tokio worker threads for accept, parse and write I/O. Defaults to
    /// the number of CPU cores (`docs/DESIGN.md` §7.1).
    pub worker_threads: usize,
    /// Maximum size of the request header block (all header fields plus the
    /// terminator, excluding the request line) in bytes. A larger block is
    /// rejected with `431`. Default: 16 KiB.
    pub max_header_block_size: usize,
    /// Maximum number of header fields. Rejected with `431`. Default: 100.
    pub max_header_count: usize,
    /// Maximum request-target (URI) length in bytes. Rejected with `414`.
    /// Default: 8 KiB.
    pub max_url_length: usize,
    /// Maximum request body size in bytes. Rejected with `413`. A declared
    /// `Content-Length` above the limit is rejected before the body is
    /// read. Default: 1 MiB.
    pub max_body_size: usize,
    /// Time allowed to read a complete request head once its first byte
    /// arrives. On expiry the server answers `408` and closes the
    /// connection. Default: 10 s.
    pub header_read_timeout: Duration,
    /// Time allowed to read a complete request body. On expiry the server
    /// answers `408` and closes the connection. Default: 30 s. The
    /// minimum-rate protection from `docs/DESIGN.md` §14.4 is M4 work.
    pub body_read_timeout: Duration,
    /// Idle keep-alive time between requests. An idle connection is closed
    /// silently after this. Default: 60 s.
    pub keep_alive_idle_timeout: Duration,
    /// Maximum concurrent connections held by this server. Beyond this the
    /// accepted socket is refused (closed without a response), per
    /// `docs/DESIGN.md` §14.3. Default: 10 000.
    pub max_connections: usize,
    /// Hard deadline for graceful shutdown: stop accepting, let in-flight
    /// connections finish, then close the rest. Default: 30 s.
    pub shutdown_timeout: Duration,
    /// Install tokio signal handlers so `SIGINT`/`SIGTERM` trigger the same
    /// graceful shutdown as [`Server::shutdown`](crate::Server::shutdown).
    /// Embedders that own signal handling (for example a Python interpreter
    /// that must observe `SIGINT` itself) should set this to `false`.
    /// Default: `true`.
    pub handle_signals: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 8000)),
            worker_threads: available_parallelism().map_or(1, std::num::NonZero::get),
            max_header_block_size: 16 * 1024,
            max_header_count: 100,
            max_url_length: 8 * 1024,
            max_body_size: 1024 * 1024,
            header_read_timeout: Duration::from_secs(10),
            body_read_timeout: Duration::from_secs(30),
            keep_alive_idle_timeout: Duration::from_secs(60),
            max_connections: 10_000,
            shutdown_timeout: Duration::from_secs(30),
            handle_signals: true,
        }
    }
}

impl ServerConfig {
    /// Check the configuration for values that cannot work.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] when a field is zero (or, for
    /// timeouts, zero-length), since each of them is a budget, not a flag.
    pub(crate) fn validate(&self) -> io::Result<()> {
        let checks = [
            ("worker_threads", self.worker_threads),
            ("max_header_block_size", self.max_header_block_size),
            ("max_header_count", self.max_header_count),
            ("max_url_length", self.max_url_length),
            ("max_body_size", self.max_body_size),
            ("max_connections", self.max_connections),
        ];
        for (name, value) in checks {
            if value == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("ServerConfig.{name} must be greater than zero"),
                ));
            }
        }
        let timeouts = [
            ("header_read_timeout", self.header_read_timeout),
            ("body_read_timeout", self.body_read_timeout),
            ("keep_alive_idle_timeout", self.keep_alive_idle_timeout),
            ("shutdown_timeout", self.shutdown_timeout),
        ];
        for (name, value) in timeouts {
            if value.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("ServerConfig.{name} must be greater than zero"),
                ));
            }
        }
        Ok(())
    }

    /// Size of hyper's read buffer in bytes. It must hold the request line
    /// plus the largest legal header block, so a head that only exceeds one
    /// of the limits reaches the service and gets the right status (414 vs
    /// 431) instead of hyper's generic oversized-head rejection.
    pub(crate) fn head_buffer_size(&self) -> usize {
        self.max_header_block_size
            .saturating_add(self.max_url_length)
            .saturating_add(64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_design() {
        let cfg = ServerConfig::default();
        assert!(cfg.worker_threads >= 1);
        assert_eq!(cfg.max_header_block_size, 16 * 1024);
        assert_eq!(cfg.max_header_count, 100);
        assert_eq!(cfg.max_url_length, 8 * 1024);
        assert_eq!(cfg.max_body_size, 1024 * 1024);
        assert_eq!(cfg.header_read_timeout, Duration::from_secs(10));
        assert_eq!(cfg.body_read_timeout, Duration::from_secs(30));
        assert_eq!(cfg.keep_alive_idle_timeout, Duration::from_secs(60));
        assert_eq!(cfg.max_connections, 10_000);
        assert_eq!(cfg.shutdown_timeout, Duration::from_secs(30));
        assert!(cfg.handle_signals);
    }

    #[test]
    fn validate_rejects_zero_limits() {
        let cfg = ServerConfig {
            max_body_size: 0,
            ..ServerConfig::default()
        };
        assert!(cfg.validate().is_err());
        let cfg = ServerConfig {
            shutdown_timeout: Duration::ZERO,
            ..ServerConfig::default()
        };
        assert!(cfg.validate().is_err());
        assert!(ServerConfig::default().validate().is_ok());
    }
}
