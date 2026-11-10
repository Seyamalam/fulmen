//! Python bindings built with `PyO3`. This crate is the only place that touches the Python C API;
//! everything else in the workspace is plain Rust so it can be tested and
//! benchmarked without an interpreter.

use pyo3::prelude::*;

/// Native core of Fulmen, exposed to Python as `fulmen._fulmen`.
#[pymodule]
mod _fulmen {
    use pyo3::prelude::*;

    #[pymodule_init]
    fn init(m: &Bound<'_, PyModule>) -> PyResult<()> {
        m.add("__version__", env!("CARGO_PKG_VERSION"))?;
        Ok(())
    }

    /// Names of the native components compiled into this build.
    #[pyfunction]
    fn native_components() -> Vec<&'static str> {
        vec![
            fulmen_core::NAME,
            fulmen_schema::NAME,
            fulmen_security::NAME,
            fulmen_server::NAME,
        ]
    }
}
