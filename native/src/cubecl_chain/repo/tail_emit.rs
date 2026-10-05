//! Tail emission passes: slot device binding for zero-copy rendering.

use cubecl::client::Client;
use cubecl::server::Handle;
use cubecl::wgpu::{AutoCompiler, WgpuServer};

use super::SlotDevice;



/// Consumes the device slot handle into a renderer `SlotDevice`.
pub(crate) fn package_slot_device(
    client: &Client,
    h_instance_slots: Handle,
    total_slots: u32,
) -> Option<SlotDevice> {
    if total_slots == 0 {
        return None;
    }
    let res = client
        .get_resource::<WgpuServer<AutoCompiler>>(h_instance_slots)
        .expect("slot buffer resource");
    let (buffer, offset) = {
        let r = res.resource();
        (r.buffer.clone(), r.offset)
    };
    assert!(
        offset % 16 == 0,
        "slot buffer's pool offset {offset} breaks the storage binding alignment"
    );
    Some(SlotDevice {
        chunk: crate::layout::DeviceSlotChunk {
            buffer,
            offset,
            slots: total_slots,
        },
        keep_alive: Box::new(res),
    })
}
