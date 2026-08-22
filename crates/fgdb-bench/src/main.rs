//! Thin CLI over the fgdb-bench library: parse the shape selector, build the
//! production runtime authority, drive the shapes, publish NDJSON.

use asupersync::Budget;
use asupersync::runtime::RuntimeBuilder;
use fgdb_bench::{emit, run_shape};
use fgdb_types::context::PurposeContexts;

fn main() {
    let which = std::env::args().nth(1).unwrap_or_else(|| "all".to_string());
    let runtime = RuntimeBuilder::new().build().expect("runtime builds");
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let cx = PurposeContexts::narrow_runtime_root(&root).commit();

    let logical_cores = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(0);
    let cpu_model = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|line| line.starts_with("model name"))
                .and_then(|line| line.split(':').nth(1))
                .map(|tail| tail.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());
    emit(
        "bench_header",
        &[
            ("tool", "fgdb-bench".to_string()),
            ("cpu_model", cpu_model),
            ("logical_cores", logical_cores.to_string()),
            ("durability", "real-durable-path".to_string()),
            ("empirical_gate_activated", "false".to_string()),
            (
                "note",
                "machine-local baseline; no pinned manifest; numbers are publication, not gate results"
                    .to_string(),
            ),
        ],
    );

    let selected: Vec<&str> = if which == "all" {
        vec![
            "ingest-power-law",
            "point-reads-supernode",
            "version-chain",
            "cold-reopen",
            "compaction-under-load",
        ]
    } else {
        vec![&which]
    };

    let failures = runtime.block_on(async {
        let mut failures = 0;
        for name in selected {
            match run_shape(name, &cx).await {
                Ok(()) => {}
                Err(error) if error.starts_with("ENGINE_LIMIT: ") => {
                    // The harness worked; the engine hit a documented limit.
                    // Published as a first-class outcome, not a harness error.
                    emit(
                        "shape_engine_limit",
                        &[
                            ("shape", name.to_string()),
                            ("detail", error["ENGINE_LIMIT: ".len()..].to_string()),
                            ("empirical_gate_activated", "false".to_string()),
                        ],
                    );
                }
                Err(error) => {
                    failures += 1;
                    emit(
                        "shape_failed",
                        &[("shape", name.to_string()), ("error", error)],
                    );
                }
            }
        }
        failures
    });
    if failures > 0 {
        eprintln!("{failures} shape(s) failed");
        std::process::exit(1);
    }
    emit("bench_footer", &[("outcome", "pass".to_string())]);
}
