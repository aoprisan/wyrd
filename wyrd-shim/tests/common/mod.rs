//! Helpers shared by the shim integration tests. Each test lives in its own
//! file (= its own process): `wyrd_shim::init` is once-per-process.

use wyrd_core::Recording;

pub fn temp_path(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("wyrd-shim-{tag}-{}.wyrd", std::process::id()));
    p
}

/// Every park episode, as `(task, resource type, op)` triples.
pub fn parks(rec: &Recording) -> Vec<(String, String, String)> {
    rec.stats(100)
        .expect("stats")
        .longest_parks
        .into_iter()
        .map(|p| (p.task.label(), p.resource.concrete_type, p.op_name))
        .collect()
}
