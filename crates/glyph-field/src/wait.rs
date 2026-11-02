//! Every blocking wait on the device, bounded (2026-11-01).
//!
//! A wait with no limit (`PollType::Wait { timeout: None }`) on a device
//! that never finishes holds its thread forever, and with it the check
//! battery: on 2026-11-01, on monolith1, the renderer's unit-test binary
//! stopped making progress four times in half an hour with one GPU test
//! spinning and four more waiting on it, and one run reached 353 s before
//! someone killed it. Here a wait has a limit, and past it the wait panics
//! naming what it waited for. The renderer reads its limit from
//! `[gpu] wait_timeout_secs` (config/defaults.toml) and passes it in.
//!
//! What this cannot bound: the waits inside wgpu itself (a queue waits for
//! its device to go idle when it drops, and adapter and device creation
//! block in the driver). The check battery's own backstop is the runner's
//! limit on every process a gate runs (build.toml `[settings]
//! process_timeout_secs`).

use std::time::Duration;

/// Block until `submission` (the device's latest when `None`) has
/// completed and its callbacks have run, for at most `limit`.
///
/// # Panics
/// Past `limit`, naming `what` and the limit: a device that does not finish
/// a submission in that time is treated as hung, never waited on forever.
/// On any other poll error, naming `what` and the error.
pub fn wait_for(device: &wgpu::Device, submission: Option<wgpu::SubmissionIndex>, limit: Duration, what: &str) {
    match device.poll(wgpu::PollType::Wait { submission_index: submission, timeout: Some(limit) }) {
        Ok(_) => {}
        Err(wgpu::PollError::Timeout) => panic!(
            "GPU wait timed out after {} s: {what}. The device did not finish the submission in \
             that time; it is treated as hung ([gpu] wait_timeout_secs is the limit)",
            limit.as_secs_f64()
        ),
        Err(e) => panic!("GPU wait failed: {what}: {e}"),
    }
}

/// Map `slice` for reading and block until it is mapped, for at most
/// `limit` (`wait_for` on the device's latest submission, which must carry
/// the copy into the slice's buffer).
///
/// # Panics
/// As `wait_for`, and when the map itself fails, naming `what`.
pub fn map_read(device: &wgpu::Device, slice: &wgpu::BufferSlice<'_>, limit: Duration, what: &str) {
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    wait_for(device, None, limit, what);
    // The poll that saw the submission complete ran the map's callback; a
    // callback that never came is bounded too.
    match rx.recv_timeout(limit) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("GPU readback map failed: {what}: {e}"),
        Err(e) => panic!("GPU readback map callback never ran: {what}: {e}"),
    }
}
