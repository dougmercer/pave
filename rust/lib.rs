//! All preprocessing work runs in Rust, with the GIL released for the entire build.
mod engine;
mod index;
mod pbf;
mod storage;
mod tags;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use std::collections::HashMap;
use std::path::PathBuf;

#[pyfunction]
#[allow(clippy::too_many_arguments)]
fn build(
    py: Python<'_>,
    input: PathBuf,
    output: PathBuf,
    scratch: PathBuf,
    tile_degrees: f64,
    spool_mb: usize,
    threads: usize,
    quiet: bool,
) -> PyResult<String> {
    let config = engine::Config {
        input,
        output,
        scratch,
        degrees: tile_degrees,
        spool_mb,
        threads,
        quiet,
        interrupted: Box::new(|| Python::attach(|py| py.check_signals()).map_err(Into::into)),
    };
    py.detach(move || engine::run(config)).map_err(|error| {
        error
            .downcast::<PyErr>()
            .unwrap_or_else(|error| PyValueError::new_err(format!("{error:#}")))
    })
}

fn borrowed(tags: &HashMap<String, String>) -> Vec<(&str, &str)> {
    tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
}

#[pyfunction(name = "width_m")]
fn width(tags: HashMap<String, String>) -> f64 {
    tags::width(&borrowed(&tags))
}

#[pyfunction]
fn direction(tags: HashMap<String, String>) -> i8 {
    tags::direction(&borrowed(&tags))
}

#[pyfunction]
fn flags(tags: HashMap<String, String>) -> u8 {
    tags::flags(&borrowed(&tags))
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
