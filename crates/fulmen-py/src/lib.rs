//! Python bindings built with `PyO3`. This crate is the only place that touches the Python C API;
//! everything else in the workspace is plain Rust so it can be tested and
//! benchmarked without an interpreter.
//!
//! Layout: [`json`] converts Python values to a serializable tree, [`pool`]
//! is the bounded sync-handler thread pool, [`aloop`] is the asyncio
//! loop-thread bridge for `async def` handlers, [`convert`] calls handlers
//! and maps their return values to wire responses, [`dispatch`] owns the
//! HTTP semantics over the compiled route table, [`app`] records and
//! compiles declarations, and [`serve`] exposes the two entry points
//! (`serve`/`serve_for_test`).

// mimalloc is the global allocator of the extension module (DESIGN.md
// §25.2). It is target-gated exactly like the dependency in Cargo.toml so
// platforms without mimalloc keep the system allocator.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

use pyo3::prelude::*;

mod aloop;
mod app;
mod convert;
mod descriptor;
mod dispatch;
mod encode;
mod extract;
mod hooks;
mod json;
mod plugins;
mod pool;
mod serve;

/// Native core of Fulmen, exposed to Python as `fulmen._fulmen`.
#[pymodule]
mod _fulmen {
    use pyo3::prelude::*;

    #[pymodule_init]
    fn init(m: &Bound<'_, PyModule>) -> PyResult<()> {
        m.add("__version__", env!("CARGO_PKG_VERSION"))?;
        m.add(
            "StartupError",
            m.py().get_type::<crate::app::StartupError>(),
        )?;
        m.add_class::<crate::app::NativeApp>()?;
        m.add_class::<crate::descriptor::Descriptor>()?;
        m.add_class::<crate::serve::TestServer>()?;
        m.add_function(pyo3::wrap_pyfunction!(crate::serve::serve, m)?)?;
        m.add_function(pyo3::wrap_pyfunction!(crate::serve::serve_for_test, m)?)?;
        m.add_function(pyo3::wrap_pyfunction!(
            crate::descriptor::compile_descriptor,
            m
        )?)?;
        m.add_function(pyo3::wrap_pyfunction!(
            crate::plugins::resolve_plugin_order,
            m
        )?)?;
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
