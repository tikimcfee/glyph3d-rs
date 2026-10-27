//! What a load may ask of the machine, checked before it asks (2026-10-26).
//!
//! A load past what the device or the host can hold is refused with the
//! numbers, after Pass 1 has counted the slots and before Pass 2 or the
//! field allocates any of them. Without this a 1.28B-slot tree on a 32 GB
//! card got its buffer creations refused by the driver one by one; wgpu
//! logs an uncaptured error and hands back an invalid buffer, and the load
//! died later, on a worker thread, mapping one of them.
//!
//! The device's numbers are the driver's own: on Vulkan,
//! `VK_EXT_memory_budget` for every device-local heap (the budget is what
//! this process may allocate there now, other processes' use already
//! subtracted; the usage is this process's). No other backend reports a
//! budget through wgpu, and there the device side goes unchecked, which the
//! demand line says. The host's number is `MemAvailable` (Linux). On a
//! unified-memory device (an integrated GPU, Apple silicon) the device's
//! memory is the host's: the host blocks and the device copy are one
//! demand on one pool, held to the smaller of the two numbers.

/// A device's local memory as its driver reports it, summed over its
/// device-local heaps.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeviceMemory {
    pub heap_bytes: u64,
    pub budget_bytes: u64,
    pub usage_bytes: u64,
}

impl DeviceMemory {
    /// What this process may still allocate there.
    pub fn free_bytes(&self) -> u64 {
        self.budget_bytes.saturating_sub(self.usage_bytes)
    }
}

/// `adapter`'s device-local memory, where the driver reports a budget.
#[cfg(not(any(target_vendor = "apple", target_family = "wasm")))]
pub fn device_memory(adapter: &wgpu::Adapter) -> Option<DeviceMemory> {
    use ash::vk;
    // SAFETY: the hal adapter is used only within this call, while `adapter`
    // is alive, for one read-only physical-device query.
    let hal = unsafe { adapter.as_hal::<wgpu::hal::api::Vulkan>() }?;
    let instance = hal.shared_instance().raw_instance();
    // SAFETY: the physical device is the hal adapter's, enumerated from
    // `instance`, which outlives the call.
    let supported = unsafe { instance.enumerate_device_extension_properties(hal.raw_physical_device()) }.ok()?;
    if !supported.iter().any(|e| e.extension_name_as_c_str() == Ok(ash::ext::memory_budget::NAME)) {
        return None;
    }
    let mut budget = vk::PhysicalDeviceMemoryBudgetPropertiesEXT::default();
    let mut props = vk::PhysicalDeviceMemoryProperties2::default().push_next(&mut budget);
    // SAFETY: the physical device is the hal adapter's, the extension is
    // supported (above), and both structs live through the call.
    unsafe { instance.get_physical_device_memory_properties2(hal.raw_physical_device(), &mut props) };
    let heaps = props.memory_properties.memory_heaps_as_slice();
    let mut memory = DeviceMemory { heap_bytes: 0, budget_bytes: 0, usage_bytes: 0 };
    for (i, heap) in heaps.iter().enumerate() {
        if heap.flags.contains(vk::MemoryHeapFlags::DEVICE_LOCAL) {
            memory.heap_bytes += heap.size;
            memory.budget_bytes += budget.heap_budget[i];
            memory.usage_bytes += budget.heap_usage[i];
        }
    }
    (memory.heap_bytes > 0).then_some(memory)
}

/// No backend but Vulkan reports a budget through wgpu.
#[cfg(any(target_vendor = "apple", target_family = "wasm"))]
pub fn device_memory(_adapter: &wgpu::Adapter) -> Option<DeviceMemory> {
    None
}

/// The host memory available to a new allocation (`MemAvailable`; Linux
/// only, None elsewhere).
pub fn host_available_bytes() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = meminfo.lines().find(|l| l.starts_with("MemAvailable:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

/// What a load's slots ask for: `slots` of `slot_bytes`, held on the host
/// in blocks (`host_blocks`, a Derived load) and on the device (`device`,
/// plus `staging` in flight while they go up).
#[derive(Clone, Copy, Debug)]
pub struct SlotDemand {
    pub slots: u64,
    pub slot_bytes: u64,
    pub block_slots: u64,
    pub host_blocks: bool,
    pub device: bool,
    pub staging: u64,
}

impl SlotDemand {
    pub fn slot_total(&self) -> u64 {
        self.slots * self.slot_bytes
    }
}

/// The machine's side: the host's available memory and the device's, where
/// known, and whether they are one memory (`unified`).
#[derive(Clone, Debug)]
pub struct Machine {
    pub host_available: Option<u64>,
    pub device_name: String,
    pub device: Option<DeviceMemory>,
    pub unified: bool,
}

impl Machine {
    pub fn of(adapter: &wgpu::Adapter, unified: bool) -> Self {
        Self {
            host_available: host_available_bytes(),
            device_name: adapter.get_info().name,
            device: device_memory(adapter),
            unified,
        }
    }
}

fn gib(bytes: u64) -> String {
    format!("{:.2} GiB", bytes as f64 / f64::from(1u32 << 30))
}

/// The demand against the machine: `Ok` with the line that states both,
/// `Err` with that line and every limit it is past.
pub fn check(demand: &SlotDemand, machine: &Machine) -> Result<String, String> {
    let total = demand.slot_total();
    let mut line = format!(
        "{} slots x {} B = {} of glyph slots",
        demand.slots,
        demand.slot_bytes,
        gib(total)
    );
    if demand.host_blocks {
        let blocks = demand.slots.div_ceil(demand.block_slots).max(1);
        line += &format!(
            " in {blocks} host blocks of {:.1} MiB",
            (demand.block_slots * demand.slot_bytes) as f64 / f64::from(1u32 << 20)
        );
    }
    let mut past = Vec::new();
    if demand.host_blocks {
        match machine.host_available {
            Some(avail) => {
                line += &format!("; host: {} available", gib(avail));
                if total > avail {
                    past.push(format!("the host blocks need {}, {} more than the host has available", gib(total), gib(total - avail)));
                }
            }
            None => line += "; host: available memory unknown, not checked",
        }
    }
    if demand.device {
        let need = total + demand.staging;
        match machine.device {
            Some(dev) => {
                line += &format!(
                    "; device ({}): {} of slot buffers and upload staging, local heap {}, budget {}, {} in use, {} free",
                    machine.device_name,
                    gib(need),
                    gib(dev.heap_bytes),
                    gib(dev.budget_bytes),
                    gib(dev.usage_bytes),
                    gib(dev.free_bytes())
                );
                if need > dev.free_bytes() {
                    past.push(format!(
                        "the device needs {}, {} more than its budget leaves free",
                        gib(need),
                        gib(need - dev.free_bytes())
                    ));
                }
            }
            None => line += &format!("; device ({}): no budget reported, not checked", machine.device_name),
        }
    }
    if machine.unified && demand.host_blocks && demand.device {
        // One memory: the separate checks above each see half the demand.
        let need = total + total + demand.staging;
        let pool = [machine.host_available, machine.device.map(|d| d.free_bytes())].into_iter().flatten().min();
        match pool {
            Some(pool) => {
                line += &format!("; unified memory: host blocks and device copy {} of one pool, {} free", gib(need), gib(pool));
                if need > pool {
                    past.push(format!(
                        "on unified memory the host blocks and the device copy need {} together, {} more than the pool has free",
                        gib(need),
                        gib(need - pool)
                    ));
                }
            }
            None => line += "; unified memory: pool unknown, not checked",
        }
    }
    if past.is_empty() {
        Ok(line)
    } else {
        Err(format!("{line}. Refused: {}", past.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn linux_tree() -> SlotDemand {
        SlotDemand { slots: 1_275_000_000, slot_bytes: 20, block_slots: 262_144, host_blocks: true, device: true, staging: GIB / 2 }
    }

    fn machine(host: u64, budget: u64, usage: u64) -> Machine {
        Machine {
            host_available: Some(host),
            device_name: "a 32 GB card".into(),
            device: Some(DeviceMemory { heap_bytes: 32 * GIB, budget_bytes: budget, usage_bytes: usage }),
            unified: false,
        }
    }

    #[test]
    fn a_load_past_the_device_budget_is_refused_with_its_numbers() {
        let err = check(&linux_tree(), &machine(48 * GIB, 28 * GIB, 6 * GIB)).unwrap_err();
        assert!(err.contains("1275000000 slots x 20 B = 23.75 GiB"), "{err}");
        assert!(err.contains("in 4864 host blocks of 5.0 MiB"), "{err}");
        assert!(err.contains("budget 28.00 GiB, 6.00 GiB in use, 22.00 GiB free"), "{err}");
        assert!(err.contains("Refused: the device needs 24.25 GiB, 2.25 GiB more than its budget leaves free"), "{err}");
        assert!(!err.contains("host blocks need"), "the host fits: {err}");
    }

    #[test]
    fn host_and_device_are_each_checked_and_unknowns_said() {
        assert!(check(&linux_tree(), &machine(48 * GIB, 28 * GIB, 0)).is_ok());
        let err = check(&linux_tree(), &machine(20 * GIB, 28 * GIB, 0)).unwrap_err();
        assert!(err.contains("Refused: the host blocks need 23.75 GiB, 3.75 GiB more than the host has available"), "{err}");
        let host_only = SlotDemand { device: false, ..linux_tree() };
        assert!(check(&host_only, &machine(48 * GIB, GIB, 0)).is_ok(), "a host-drawn field asks nothing of the device");
        let unknown = Machine { host_available: None, device_name: "a card".into(), device: None, unified: false };
        let line = check(&linux_tree(), &unknown).unwrap();
        assert!(line.contains("host: available memory unknown, not checked") && line.contains("no budget reported, not checked"), "{line}");
    }

    /// The review's case (2026-10-26): a 32 GB APU with 24 GiB available
    /// and a 20 GiB device budget, and a 15 GiB Derived load. Each side
    /// alone fits; together, from the one memory, they do not.
    #[test]
    fn on_unified_memory_the_host_blocks_and_device_copy_are_one_demand() {
        let load = SlotDemand { slots: 15 * GIB / 20, slot_bytes: 20, block_slots: 262_144, host_blocks: true, device: true, staging: GIB / 2 };
        let discrete = machine(24 * GIB, 20 * GIB, 0);
        assert!(check(&load, &discrete).is_ok(), "on a discrete card the two pools are separate");
        let apu = Machine { unified: true, ..discrete };
        let err = check(&load, &apu).unwrap_err();
        assert!(
            err.contains("Refused: on unified memory the host blocks and the device copy need 30.50 GiB together, 10.50 GiB more than the pool has free"),
            "{err}"
        );
        let host_drawn = SlotDemand { device: false, ..load };
        assert!(check(&host_drawn, &apu).is_ok(), "a host-drawn field has no device copy to add");
    }
}
