//! All preprocessing work runs in Rust, with the GIL released for the entire build.
mod engine;
mod index;
mod storage;
mod tags;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use std::path::PathBuf;

#[pyfunction]
#[allow(clippy::too_many_arguments)]
fn build(
    py: Python<'_>,
    input: PathBuf,
    output: PathBuf,
    scratch: PathBuf,
    tile_degrees: f64,
    cache_mb: usize,
    threads: usize,
    quiet: bool,
) -> PyResult<String> {
    let config = engine::Config {
        input,
        output,
        scratch,
        degrees: tile_degrees,
        cache_mb,
        threads,
        quiet,
    };
    py.detach(move || engine::run(config))
        .map_err(|error| PyValueError::new_err(format!("{error:#}")))
}

#[pyfunction(name = "width_m")]
fn width(tags: tags::Tags) -> f64 {
    tags::width(&tags)
}

#[pyfunction]
fn direction(tags: tags::Tags) -> i8 {
    tags::direction(&tags)
}

#[pyfunction]
fn flags(tags: tags::Tags) -> u8 {
    tags::flags(&tags)
}

#[pyfunction]
fn scalar(value: Option<&str>, kind: &str) -> PyResult<f64> {
    if !matches!(kind, "length" | "speed" | "weight" | "lanes") {
        return Err(PyValueError::new_err("Unknown numeric tag kind"));
    }
    Ok(tags::scalar(value, kind))
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(build, module)?)?;
    module.add_function(wrap_pyfunction!(width, module)?)?;
    module.add_function(wrap_pyfunction!(direction, module)?)?;
    module.add_function(wrap_pyfunction!(flags, module)?)?;
    module.add_function(wrap_pyfunction!(scalar, module)?)?;
    Ok(())
}
