//! Fulmen security: CORS, CSRF, rate limiting, JWT verification, sessions
//! and secure headers, all evaluated natively in the request pipeline.

/// Component name, reported by `fulmen._fulmen.native_components()`.
pub const NAME: &str = "security";
