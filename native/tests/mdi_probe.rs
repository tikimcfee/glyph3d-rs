//! Stage K: run the `mdi-probe` proof binary under `cargo test`, so the
//! indirect-draw first_instance contract this stage is built on — a spec
//! no-op without `Features::INDIRECT_FIRST_INSTANCE`, real draws with it —
//! is re-verified on the adapter every time the tests run. The probe is
//! self-contained (its own instance/device per configuration); see
//! src/bin/mdi_probe.rs for the matrix.

#[test]
fn indirect_first_instance_matrix_holds() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_mdi-probe"))
        .output()
        .expect("spawn mdi-probe");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "mdi-probe exited {:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        out.status.code()
    );
    assert!(
        stdout.contains("RESULT: PASS"),
        "mdi-probe did not report PASS:\n{stdout}"
    );
}
