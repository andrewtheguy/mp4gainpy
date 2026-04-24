use std::path::Path;

use pyo3::{exceptions::PyRuntimeError, prelude::*, types::PyBytes};

#[pyfunction]
#[pyo3(name = "aac_apply_gain")]
fn aac_apply_gain_py<'py>(
    py: Python<'py>,
    data: &[u8],
    gain_steps: i32,
) -> PyResult<Bound<'py, PyBytes>> {
    let out = crate::aac_apply_gain(data, gain_steps)
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
    Ok(PyBytes::new(py, &out))
}

#[pyfunction]
#[pyo3(name = "aac_apply_gain_file")]
fn aac_apply_gain_file_py(
    src_path: &str,
    dst_path: &str,
    gain_steps: i32,
) -> PyResult<usize> {
    crate::aac_apply_gain_file(Path::new(src_path), Path::new(dst_path), gain_steps)
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))
}

#[pymodule]
#[pyo3(name = "mp4gainpy")]
fn mp4gainpy_module(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add(
        "__all__",
        vec!["aac_apply_gain", "aac_apply_gain_file", "GAIN_STEP_DB"],
    )?;
    module.add("GAIN_STEP_DB", crate::GAIN_STEP_DB)?;
    module.add_function(wrap_pyfunction!(aac_apply_gain_py, module)?)?;
    module.add_function(wrap_pyfunction!(aac_apply_gain_file_py, module)?)?;
    Ok(())
}
