//! Vulkan compute for the search: a device, and a search session that owns everything the genetic
//! algorithm needs on it.
//!
//! The search runs entirely in compute shaders (`shaders/`, compiled to SPIR-V by `build.rs`). A
//! [`Session`] uploads the old and the new image **once**, as read-only storage buffers that every
//! stage reads, and allocates the search's state next to them: the population and the finalist
//! archive, the replay and scan workspaces, the copy-source tables and the control words. Between
//! batches the host reads back only the control words. Chromosomes are read back when the search
//! ends. Nothing about an individual candidate crosses to the host while the search runs.
//!
//! **Memory.** The images, the state and the tables are device-local. On a discrete GPU that is
//! VRAM, reached through a staging buffer; on an integrated GPU it is shared memory that the
//! driver still reports as device-local, and on the software device (Mesa lavapipe, used by the
//! tests) it is ordinary memory. Only the staging and readback buffers are host-visible.
//!
//! **Limits.** Offsets and lengths on the device are 32 bits, so an image must be smaller than
//! [`MAX_IMAGE`](crate::gpu_layout::MAX_IMAGE) bytes, and every buffer must fit the device's
//! `maxStorageBufferRange`. The session checks the whole allocation before it allocates anything
//! and refuses an image it cannot hold, with the limit that failed. It does not fall back to
//! streamed windows.
//!
//! **Dispatch.** A generation is several compute stages with a memory barrier between each. A
//! batch records several generations into one command buffer and waits once, so the host controls
//! the time per submission and reads one small record afterwards. Every random choice is a pure
//! function of the seed, the generation and the candidate, so splitting a run into batches
//! does not change its result.
//!
//! **Devices.** Hardware compute devices are tried first, in the order discrete, integrated,
//! virtual. Each is attempted in turn, and when none can be initialized the software `CPU`
//! device is attempted. [`DevicePreference::Software`] tries only the software device. Each failed
//! attempt is recorded with its device name in a [`DeviceFailure`]. When no device works the open
//! fails with installation instructions. There is no non-Vulkan fallback.
//!
//! **Ownership.** Every Vulkan object is held by a wrapper that releases it on drop, so a failure
//! at any step leaves nothing allocated. A submitted batch is waited for before the call returns,
//! and the device is drained before it is released.
//!
//! **Lifecycle lock.** The Vulkan loader and the drivers behind it keep process-wide state, so
//! opening a device (loading the library, creating the instance, enumerating and creating the
//! device) and releasing one (destroying the device and the instance, and unloading the library) each
//! hold one process-wide mutex. Work on an open device does not: submissions from different
//! [`Gpu`]s run in parallel.

use std::ffi::CStr;
use std::io::Cursor;
use std::ptr;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::time::Duration;
use std::time::Instant;

use ash::vk;
use ash::vk::Handle;
use delta_update_payload::error::Code;
use delta_update_payload::error::PayloadError;
use delta_update_payload::error::err;

use crate::gpu_layout::*;
use crate::program::Op;

mod stages {
    include!(concat!(env!("OUT_DIR"), "/gpu_stages.rs"));
}

const NO_LOADER: &str = "no Vulkan loader: install libvulkan1 and a driver. For a software device, install mesa-vulkan-drivers (lavapipe). `vulkaninfo --summary` shows what is present.";
const NO_DEVICE: &str = "no Vulkan compute device: install a hardware driver, or mesa-vulkan-drivers for the software device lavapipe. `vulkaninfo --summary` shows what is present.";
const NO_SOFTWARE: &str = "no software Vulkan device: install mesa-vulkan-drivers (lavapipe) and libvulkan1. If it is installed, check that its ICD file is in /usr/share/vulkan/icd.d, or set VK_ICD_FILENAMES to it.";
const NO_USABLE: &str = "no Vulkan device could be initialized: each device was tried and failed. The attempts are listed in the device failure.";

const PUSH_WORDS: usize = 5;
const STAGING_BYTES: usize = 16 << 20;
const READBACK_BYTES: usize = 4 << 20;
const STAGING_MIN: usize = 1 << 20;
const SCAN_BYTES: usize = 8192;
const BINDINGS: u32 = 4;

/// Which device the search runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevicePreference {
    /// Hardware compute devices in order, then the software device if they all fail.
    Auto,
    /// The software device (`CPU` type, such as lavapipe), even when hardware exists.
    Software,
}

/// One compute stage of the search. Each is its own SPIR-V module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    IdxFill,
    IdxSort,
    Classify,
    CellsCount,
    CellsScan,
    CellsEmit,
    Bitmaps,
    InitState,
    Seed,
    Breed,
    EvalReplay,
    EvalCount,
    EvalPrefix,
    EvalEmit,
    EvalFinish,
    Select,
}

impl Stage {
    const ALL: [Stage; 16] = [
        Stage::IdxFill,
        Stage::IdxSort,
        Stage::Classify,
        Stage::CellsCount,
        Stage::CellsScan,
        Stage::CellsEmit,
        Stage::Bitmaps,
        Stage::InitState,
        Stage::Seed,
        Stage::Breed,
        Stage::EvalReplay,
        Stage::EvalCount,
        Stage::EvalPrefix,
        Stage::EvalEmit,
        Stage::EvalFinish,
        Stage::Select,
    ];

    fn spirv(self) -> &'static [u8] {
        return match self {
            Stage::IdxFill => stages::IDX_FILL,
            Stage::IdxSort => stages::IDX_SORT,
            Stage::Classify => stages::CLASSIFY,
            Stage::CellsCount => stages::CELLS_COUNT,
            Stage::CellsScan => stages::CELLS_SCAN,
            Stage::CellsEmit => stages::CELLS_EMIT,
            Stage::Bitmaps => stages::BITMAPS,
            Stage::InitState => stages::INIT_STATE,
            Stage::Seed => stages::SEED,
            Stage::Breed => stages::BREED,
            Stage::EvalReplay => stages::EVAL_REPLAY,
            Stage::EvalCount => stages::EVAL_COUNT,
            Stage::EvalPrefix => stages::EVAL_PREFIX,
            Stage::EvalEmit => stages::EVAL_EMIT,
            Stage::EvalFinish => stages::EVAL_FINISH,
            Stage::Select => stages::SELECT,
        };
    }

    fn index(self) -> usize {
        return Stage::ALL.iter().position(|s| *s == self).unwrap_or(0);
    }
}

/// What a device allows, as far as the search is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceLimits {
    /// The most bytes one storage buffer descriptor may cover.
    pub max_buffer_range: u64,
    /// Most work groups of one dispatch along x, and along y.
    pub max_groups_x: u32,
    pub max_groups_y: u32,
}

/// One physical device the search may try, with what its attempt needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    physical: vk::PhysicalDevice,
    family: u32,
    name: String,
    kind: Kind,
    limits: DeviceLimits,
}

/// The device categories the search prefers, most preferred first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Discrete,
    Integrated,
    Virtual,
    Other,
    Software,
}

impl Kind {
    fn of(device_type: vk::PhysicalDeviceType) -> Kind {
        return match device_type {
            vk::PhysicalDeviceType::DISCRETE_GPU => Kind::Discrete,
            vk::PhysicalDeviceType::INTEGRATED_GPU => Kind::Integrated,
            vk::PhysicalDeviceType::VIRTUAL_GPU => Kind::Virtual,
            vk::PhysicalDeviceType::CPU => Kind::Software,
            _ => Kind::Other,
        };
    }
}

/// A device that could not be initialized, with the error its attempt returned.
#[derive(Debug)]
pub struct Attempt {
    pub device: String,
    pub error: PayloadError,
}

/// Why no device could be opened: the error for the caller, and every attempt that failed.
#[derive(Debug)]
pub struct DeviceFailure {
    pub error: PayloadError,
    pub attempts: Vec<Attempt>,
}

static LIFECYCLE: Mutex<()> = Mutex::new(());

fn lifecycle() -> MutexGuard<'static, ()> {
    return LIFECYCLE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
}

struct Instance {
    handle: ash::Instance,
    _entry: ash::Entry,
}

impl Drop for Instance {
    fn drop(&mut self) {
        unsafe { self.handle.destroy_instance(None) };
    }
}

struct DeviceGuard(ash::Device);

impl Drop for DeviceGuard {
    fn drop(&mut self) {
        unsafe { self.0.destroy_device(None) };
    }
}

struct Owned<H: Handle + Copy> {
    device: ash::Device,
    handle: H,
    release: fn(&ash::Device, H),
}

impl<H: Handle + Copy> Owned<H> {
    fn new(device: &ash::Device, handle: H, release: fn(&ash::Device, H)) -> Owned<H> {
        return Owned {
            device: device.clone(),
            handle: handle,
            release: release,
        };
    }
}

impl<H: Handle + Copy> Drop for Owned<H> {
    fn drop(&mut self) {
        if self.handle.as_raw() != 0 {
            (self.release)(&self.device, self.handle);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    Device,
    Host,
}

struct Buffer {
    buffer: Owned<vk::Buffer>,
    memory: Owned<vk::DeviceMemory>,
    ptr: *mut u8,
    bytes: usize,
}

impl Buffer {
    fn create(
        device: &ash::Device,
        memory: &vk::PhysicalDeviceMemoryProperties,
        bytes: usize,
        place: Place,
    ) -> Result<Buffer, PayloadError> {
        let io = |_| err(Code::device_io, "Vulkan buffer allocation failed");
        let physical = match bytes.checked_add(3) {
            None => return Err(err(Code::unsupported_field, "device buffer size overflows")),
            Some(p) => std::cmp::max(p & !3, 4),
        };
        let usage = vk::BufferUsageFlags::STORAGE_BUFFER
            | vk::BufferUsageFlags::TRANSFER_SRC
            | vk::BufferUsageFlags::TRANSFER_DST;
        let info = vk::BufferCreateInfo::default()
            .size(physical as u64)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let raw = unsafe { device.create_buffer(&info, None) }.map_err(io)?;
        let buffer = Owned::new(device, raw, release_buffer);
        let req = unsafe { device.get_buffer_memory_requirements(raw) };
        let index = match memory_type(memory, req.memory_type_bits, place) {
            None => return Err(err(Code::device_io, "no memory type suits the buffer")),
            Some(i) => i,
        };
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(index);
        let raw_memory = match unsafe { device.allocate_memory(&alloc, None) } {
            Err(_) => {
                return Err(err(
                    Code::device_io,
                    "Vulkan memory allocation failed: the device is out of memory",
                ));
            }
            Ok(m) => m,
        };
        let memory_owned = Owned::new(device, raw_memory, release_memory);
        unsafe { device.bind_buffer_memory(raw, raw_memory, 0) }.map_err(io)?;
        let mut mapped: *mut u8 = ptr::null_mut();
        if place == Place::Host {
            mapped =
                unsafe { device.map_memory(raw_memory, 0, req.size, vk::MemoryMapFlags::empty()) }
                    .map_err(io)? as *mut u8;
        }
        return Ok(Buffer {
            buffer: buffer,
            memory: memory_owned,
            ptr: mapped,
            bytes: physical,
        });
    }

    fn handle(&self) -> vk::Buffer {
        return self.buffer.handle;
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { self.memory.device.unmap_memory(self.memory.handle) };
        }
    }
}

fn release_buffer(d: &ash::Device, h: vk::Buffer) {
    unsafe { d.destroy_buffer(h, None) };
}

fn release_memory(d: &ash::Device, h: vk::DeviceMemory) {
    unsafe { d.free_memory(h, None) };
}

fn release_pipeline(d: &ash::Device, h: vk::Pipeline) {
    unsafe { d.destroy_pipeline(h, None) };
}

fn release_pipeline_layout(d: &ash::Device, h: vk::PipelineLayout) {
    unsafe { d.destroy_pipeline_layout(h, None) };
}

fn release_shader_module(d: &ash::Device, h: vk::ShaderModule) {
    unsafe { d.destroy_shader_module(h, None) };
}

fn release_descriptor_pool(d: &ash::Device, h: vk::DescriptorPool) {
    unsafe { d.destroy_descriptor_pool(h, None) };
}

fn release_descriptor_layout(d: &ash::Device, h: vk::DescriptorSetLayout) {
    unsafe { d.destroy_descriptor_set_layout(h, None) };
}

fn release_fence(d: &ash::Device, h: vk::Fence) {
    unsafe { d.destroy_fence(h, None) };
}

fn release_command_pool(d: &ash::Device, h: vk::CommandPool) {
    unsafe { d.destroy_command_pool(h, None) };
}

struct Context {
    name: String,
    limits: DeviceLimits,
    memory: vk::PhysicalDeviceMemoryProperties,
    queue: vk::Queue,
    command: vk::CommandBuffer,
    fence: Owned<vk::Fence>,
    _command_pool: Owned<vk::CommandPool>,
    _descriptor_pool: Owned<vk::DescriptorPool>,
    _descriptor_layout: Owned<vk::DescriptorSetLayout>,
    set: vk::DescriptorSet,
    pipe_layout: Owned<vk::PipelineLayout>,
    pipelines: Vec<Owned<vk::Pipeline>>,
    device: DeviceGuard,
}

impl Drop for Context {
    fn drop(&mut self) {
        let _ = unsafe { self.device.0.device_wait_idle() };
    }
}

struct Parts {
    context: Context,
    _instance: Instance,
}

/// An open Vulkan compute context: the instance, and the device the search runs on.
pub struct Gpu {
    parts: Option<Parts>,
    skipped: Vec<Attempt>,
}

impl Drop for Gpu {
    fn drop(&mut self) {
        let _lifecycle = lifecycle();
        drop(self.parts.take());
    }
}

impl Gpu {
    /// Open the device that `preference` selects. Fails with installation instructions
    /// when no device can be initialized.
    pub fn open(preference: DevicePreference) -> Result<Gpu, PayloadError> {
        return match Gpu::open_detailed(preference) {
            Err(f) => Err(f.error),
            Ok(g) => Ok(g),
        };
    }

    /// [`Gpu::open`], with the record of every device attempt that failed on the way.
    pub fn open_detailed(preference: DevicePreference) -> Result<Gpu, DeviceFailure> {
        let failure = |error: PayloadError| DeviceFailure {
            error: error,
            attempts: Vec::new(),
        };
        let _lifecycle = lifecycle();
        let entry = match unsafe { ash::Entry::load() } {
            Err(_) => return Err(failure(err(Code::no_compute_device, NO_LOADER))),
            Ok(e) => e,
        };
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_0);
        let info = vk::InstanceCreateInfo::default().application_info(&app);
        let handle = match unsafe { entry.create_instance(&info, None) } {
            Err(_) => return Err(failure(err(Code::no_compute_device, NO_LOADER))),
            Ok(i) => i,
        };
        let instance = Instance {
            handle: handle,
            _entry: entry,
        };
        let candidates = match candidates(&instance.handle, preference) {
            Err(e) => return Err(failure(e)),
            Ok(c) => c,
        };
        let (context, skipped) =
            match first_working(&candidates, |c| create_context(&instance.handle, c)) {
                Err(f) => return Err(f),
                Ok(v) => v,
            };
        return Ok(Gpu {
            parts: Some(Parts {
                context: context,
                _instance: instance,
            }),
            skipped: skipped,
        });
    }

    fn context(&self) -> &Context {
        return match &self.parts {
            Some(p) => &p.context,
            None => unreachable!("a Gpu holds its parts until it is dropped"),
        };
    }

    /// The name of the device in use, for diagnostics.
    pub fn device_name(&self) -> &str {
        return &self.context().name;
    }

    /// The devices tried before the one in use, each with the error its attempt returned.
    /// Empty when the first device tried was usable.
    pub fn skipped(&self) -> &[Attempt] {
        return &self.skipped;
    }

    /// What the device in use allows.
    pub fn limits(&self) -> DeviceLimits {
        return self.context().limits;
    }

    fn submit<F>(&self, record: F) -> Result<(), PayloadError>
    where
        F: FnOnce(&Batch) -> Result<(), PayloadError>,
    {
        let io = |_| err(Code::device_io, "Vulkan submission failed");
        let c = self.context();
        let dev = &c.device.0;
        unsafe {
            dev.reset_command_buffer(c.command, vk::CommandBufferResetFlags::empty())
                .map_err(io)?;
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            dev.begin_command_buffer(c.command, &begin).map_err(io)?;
        }
        let batch = Batch { context: c };
        let recorded = record(&batch);
        if let Err(e) = recorded {
            let _ = unsafe { dev.end_command_buffer(c.command) };
            return Err(e);
        }
        unsafe {
            batch.barrier(
                vk::PipelineStageFlags::COMPUTE_SHADER | vk::PipelineStageFlags::TRANSFER,
                vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_WRITE,
                vk::PipelineStageFlags::HOST,
                vk::AccessFlags::HOST_READ,
            );
            dev.end_command_buffer(c.command).map_err(io)?;
            dev.reset_fences(&[c.fence.handle]).map_err(io)?;
            let commands = [c.command];
            let submit = vk::SubmitInfo::default().command_buffers(&commands);
            dev.queue_submit(c.queue, &[submit], c.fence.handle)
                .map_err(io)?;
            dev.wait_for_fences(&[c.fence.handle], true, u64::MAX)
                .map_err(io)?;
        }
        return Ok(());
    }
}

struct Batch<'a> {
    context: &'a Context,
}

impl<'a> Batch<'a> {
    unsafe fn barrier(
        &self,
        src_stage: vk::PipelineStageFlags,
        src: vk::AccessFlags,
        dst_stage: vk::PipelineStageFlags,
        dst: vk::AccessFlags,
    ) {
        let barrier = vk::MemoryBarrier::default()
            .src_access_mask(src)
            .dst_access_mask(dst);
        unsafe {
            self.context.device.0.cmd_pipeline_barrier(
                self.context.command,
                src_stage,
                dst_stage,
                vk::DependencyFlags::empty(),
                &[barrier],
                &[],
                &[],
            )
        };
    }

    fn compute_barrier(&self) {
        unsafe {
            self.barrier(
                vk::PipelineStageFlags::COMPUTE_SHADER | vk::PipelineStageFlags::TRANSFER,
                vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_WRITE,
                vk::PipelineStageFlags::COMPUTE_SHADER | vk::PipelineStageFlags::TRANSFER,
                vk::AccessFlags::SHADER_READ
                    | vk::AccessFlags::SHADER_WRITE
                    | vk::AccessFlags::TRANSFER_READ
                    | vk::AccessFlags::TRANSFER_WRITE,
            )
        };
    }

    fn dispatch(
        &self,
        stage: Stage,
        push: [u32; PUSH_WORDS],
        groups_x: u32,
        groups_y: u32,
    ) -> Result<(), PayloadError> {
        let c = self.context;
        if groups_x > c.limits.max_groups_x || groups_y > c.limits.max_groups_y {
            return Err(err(
                Code::device_io,
                "dispatch exceeds the device workgroup limit",
            ));
        }
        if groups_x == 0 || groups_y == 0 {
            return Ok(());
        }
        let bytes: Vec<u8> = push.iter().flat_map(|w| w.to_ne_bytes()).collect();
        let dev = &c.device.0;
        unsafe {
            dev.cmd_bind_pipeline(
                c.command,
                vk::PipelineBindPoint::COMPUTE,
                c.pipelines[stage.index()].handle,
            );
            dev.cmd_bind_descriptor_sets(
                c.command,
                vk::PipelineBindPoint::COMPUTE,
                c.pipe_layout.handle,
                0,
                &[c.set],
                &[],
            );
            dev.cmd_push_constants(
                c.command,
                c.pipe_layout.handle,
                vk::ShaderStageFlags::COMPUTE,
                0,
                &bytes,
            );
            dev.cmd_dispatch(c.command, groups_x, groups_y, 1);
        }
        self.compute_barrier();
        return Ok(());
    }

    fn copy(&self, from: vk::Buffer, from_at: usize, to: vk::Buffer, to_at: usize, bytes: usize) {
        let region = vk::BufferCopy::default()
            .src_offset(from_at as u64)
            .dst_offset(to_at as u64)
            .size(bytes as u64);
        unsafe {
            self.context
                .device
                .0
                .cmd_copy_buffer(self.context.command, from, to, &[region])
        };
        self.compute_barrier();
    }

    fn zero(&self, buffer: vk::Buffer, at: usize, bytes: usize) {
        unsafe {
            self.context.device.0.cmd_fill_buffer(
                self.context.command,
                buffer,
                at as u64,
                bytes as u64,
                0,
            )
        };
        self.compute_barrier();
    }
}

fn candidates(
    instance: &ash::Instance,
    preference: DevicePreference,
) -> Result<Vec<Candidate>, PayloadError> {
    let devices = match unsafe { instance.enumerate_physical_devices() } {
        Err(_) => return Err(err(Code::no_compute_device, NO_DEVICE)),
        Ok(d) => d,
    };
    let mut found: Vec<Candidate> = Vec::new();
    for pd in devices {
        let props = unsafe { instance.get_physical_device_properties(pd) };
        let kind = Kind::of(props.device_type);
        if preference == DevicePreference::Software && kind != Kind::Software {
            continue;
        }
        let family = match compute_family(instance, pd) {
            None => continue,
            Some(f) => f,
        };
        let limit = &props.limits;
        if limit.max_per_stage_descriptor_storage_buffers < BINDINGS
            || limit.max_compute_work_group_invocations < WORKGROUP as u32
            || limit.max_compute_work_group_size[0] < WORKGROUP as u32
            || (limit.max_push_constants_size as usize) < PUSH_WORDS * 4
        {
            continue;
        }
        let memory = unsafe { instance.get_physical_device_memory_properties(pd) };
        if memory_type(&memory, u32::MAX, Place::Host).is_none()
            || memory_type(&memory, u32::MAX, Place::Device).is_none()
        {
            continue;
        }
        let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        found.push(Candidate {
            physical: pd,
            family: family,
            name: name,
            kind: kind,
            limits: DeviceLimits {
                max_buffer_range: limit.max_storage_buffer_range as u64,
                max_groups_x: limit.max_compute_work_group_count[0],
                max_groups_y: limit.max_compute_work_group_count[1],
            },
        });
    }
    if found.is_empty() {
        return Err(err(
            Code::no_compute_device,
            if preference == DevicePreference::Software {
                NO_SOFTWARE
            } else {
                NO_DEVICE
            },
        ));
    }
    return Ok(ordered(found));
}

fn ordered(mut found: Vec<Candidate>) -> Vec<Candidate> {
    found.sort_by_key(|c| c.kind);
    return found;
}

/// Try each candidate in order, returning the first that initializes. Every failed attempt
/// is recorded with its device name, so a total failure says what was tried.
pub fn first_working<T>(
    candidates: &[Candidate],
    mut attempt: impl FnMut(&Candidate) -> Result<T, PayloadError>,
) -> Result<(T, Vec<Attempt>), DeviceFailure> {
    let mut failed: Vec<Attempt> = Vec::new();
    for c in candidates {
        match attempt(c) {
            Ok(v) => return Ok((v, failed)),
            Err(e) => failed.push(Attempt {
                device: c.name.clone(),
                error: e,
            }),
        }
    }
    let error = err(Code::no_compute_device, NO_USABLE);
    return Err(DeviceFailure {
        error: error,
        attempts: failed,
    });
}

fn compute_family(instance: &ash::Instance, pd: vk::PhysicalDevice) -> Option<u32> {
    let families = unsafe { instance.get_physical_device_queue_family_properties(pd) };
    for (i, f) in families.iter().enumerate() {
        if f.queue_flags.contains(vk::QueueFlags::COMPUTE) {
            return Some(i as u32);
        }
    }
    return None;
}

fn memory_type(props: &vk::PhysicalDeviceMemoryProperties, bits: u32, place: Place) -> Option<u32> {
    let visible = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    let local = vk::MemoryPropertyFlags::DEVICE_LOCAL;
    let allowed = |i: usize| i < 32 && bits & (1u32 << i) != 0;
    let count = props.memory_type_count as usize;
    match place {
        Place::Host => {
            for i in 0..count {
                if allowed(i) && props.memory_types[i].property_flags.contains(visible) {
                    return Some(i as u32);
                }
            }
            return None;
        }
        Place::Device => {
            for i in 0..count {
                let flags = props.memory_types[i].property_flags;
                if allowed(i)
                    && flags.contains(local)
                    && !flags.contains(vk::MemoryPropertyFlags::HOST_VISIBLE)
                {
                    return Some(i as u32);
                }
            }
            for i in 0..count {
                if allowed(i) && props.memory_types[i].property_flags.contains(local) {
                    return Some(i as u32);
                }
            }
            for i in 0..count {
                if allowed(i) {
                    return Some(i as u32);
                }
            }
            return None;
        }
    }
}

fn create_context(instance: &ash::Instance, c: &Candidate) -> Result<Context, PayloadError> {
    let io = |_| err(Code::device_io, "Vulkan setup failed");
    let qci = [vk::DeviceQueueCreateInfo::default()
        .queue_family_index(c.family)
        .queue_priorities(&[1.0])];
    let dci = vk::DeviceCreateInfo::default().queue_create_infos(&qci);
    let raw = match unsafe { instance.create_device(c.physical, &dci, None) } {
        Err(_) => return Err(err(Code::device_io, "cannot create the Vulkan device")),
        Ok(d) => d,
    };
    let device = DeviceGuard(raw);
    let dev = &device.0;
    let queue = unsafe { dev.get_device_queue(c.family, 0) };

    let pool_info = vk::CommandPoolCreateInfo::default()
        .queue_family_index(c.family)
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
    let command_pool = Owned::new(
        dev,
        unsafe { dev.create_command_pool(&pool_info, None) }.map_err(io)?,
        release_command_pool,
    );
    let alloc = vk::CommandBufferAllocateInfo::default()
        .command_pool(command_pool.handle)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    let command = unsafe { dev.allocate_command_buffers(&alloc) }.map_err(io)?[0];
    let fence = Owned::new(
        dev,
        unsafe { dev.create_fence(&vk::FenceCreateInfo::default(), None) }.map_err(io)?,
        release_fence,
    );

    let bindings: Vec<vk::DescriptorSetLayoutBinding> = (0..BINDINGS)
        .map(|b| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(b)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
        })
        .collect();
    let layout_info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
    let descriptor_layout = Owned::new(
        dev,
        unsafe { dev.create_descriptor_set_layout(&layout_info, None) }.map_err(io)?,
        release_descriptor_layout,
    );
    let sizes = [vk::DescriptorPoolSize::default()
        .ty(vk::DescriptorType::STORAGE_BUFFER)
        .descriptor_count(BINDINGS)];
    let pool_info = vk::DescriptorPoolCreateInfo::default()
        .max_sets(1)
        .pool_sizes(&sizes);
    let descriptor_pool = Owned::new(
        dev,
        unsafe { dev.create_descriptor_pool(&pool_info, None) }.map_err(io)?,
        release_descriptor_pool,
    );
    let set_layouts = [descriptor_layout.handle];
    let alloc = vk::DescriptorSetAllocateInfo::default()
        .descriptor_pool(descriptor_pool.handle)
        .set_layouts(&set_layouts);
    let set = unsafe { dev.allocate_descriptor_sets(&alloc) }.map_err(io)?[0];

    let range = [vk::PushConstantRange::default()
        .stage_flags(vk::ShaderStageFlags::COMPUTE)
        .offset(0)
        .size((PUSH_WORDS * 4) as u32)];
    let pipe_info = vk::PipelineLayoutCreateInfo::default()
        .set_layouts(&set_layouts)
        .push_constant_ranges(&range);
    let pipe_layout = Owned::new(
        dev,
        unsafe { dev.create_pipeline_layout(&pipe_info, None) }.map_err(io)?,
        release_pipeline_layout,
    );
    let mut pipelines: Vec<Owned<vk::Pipeline>> = Vec::new();
    for stage in Stage::ALL {
        pipelines.push(pipeline(dev, pipe_layout.handle, stage.spirv())?);
    }
    let memory = unsafe { instance.get_physical_device_memory_properties(c.physical) };

    return Ok(Context {
        name: c.name.clone(),
        limits: c.limits,
        memory: memory,
        queue: queue,
        command: command,
        fence: fence,
        _command_pool: command_pool,
        _descriptor_pool: descriptor_pool,
        _descriptor_layout: descriptor_layout,
        set: set,
        pipe_layout: pipe_layout,
        pipelines: pipelines,
        device: device,
    });
}

fn pipeline(
    dev: &ash::Device,
    layout: vk::PipelineLayout,
    spv: &[u8],
) -> Result<Owned<vk::Pipeline>, PayloadError> {
    let io = |_| err(Code::device_io, "Vulkan pipeline creation failed");
    let code = match ash::util::read_spv(&mut Cursor::new(spv)) {
        Err(_) => return Err(err(Code::device_io, "embedded SPIR-V is malformed")),
        Ok(c) => c,
    };
    let module_info = vk::ShaderModuleCreateInfo::default().code(&code);
    let module = Owned::new(
        dev,
        unsafe { dev.create_shader_module(&module_info, None) }.map_err(io)?,
        release_shader_module,
    );
    let stage = vk::PipelineShaderStageCreateInfo::default()
        .stage(vk::ShaderStageFlags::COMPUTE)
        .module(module.handle)
        .name(c"main");
    let info = vk::ComputePipelineCreateInfo::default()
        .stage(stage)
        .layout(layout);
    return match unsafe { dev.create_compute_pipelines(vk::PipelineCache::null(), &[info], None) } {
        Ok(p) => Ok(Owned::new(dev, p[0], release_pipeline)),
        Err((created, _)) => {
            for p in created {
                let _ = Owned::new(dev, p, release_pipeline);
            }
            Err(err(Code::device_io, "Vulkan pipeline creation failed"))
        }
    };
}

/// The size model the shaders score with: the parts of the payload that do not depend on the
/// program, computed by the host with the real encoder, and the limits that are exact on the
/// device. The decode-memory words are the model's charge for everything except the operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SizeModel {
    /// The two signature messages together.
    pub signatures: u32,
    /// Encoded `PartitionUpdate` bytes without its operations.
    pub partition_fixed: u32,
    /// Encoded manifest bytes without its partitions and its `signatures_offset`.
    pub manifest_fixed: u32,
    /// The header bytes before the manifest.
    pub header: u32,
    /// The profile's encoded-manifest limit.
    pub max_manifest: u32,
    /// The decode model's slot, heap and transient charge before any operation, and its limit.
    pub decode_slots: u32,
    pub decode_heap: u32,
    pub decode_transient: u32,
    pub max_decoded: u32,
}

/// What a session needs besides the images.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionConfig {
    pub seed: u64,
    /// Whether the search may use copies out of the target.
    pub readable_target: bool,
    /// The profile's count limits, which are encoding-independent.
    pub max_operations: u32,
    pub max_extents_per_op: u32,
    pub max_total_extents: u32,
    pub model: SizeModel,
    /// Number of members sampled by each parent tournament.
    pub tournament: u32,
    /// Probability, in percent, of breeding an immigrant.
    pub immigrant_percent: u32,
    /// Probability, in percent, of breeding by crossover after the immigrant check.
    pub crossover_percent: u32,
    /// Number of best population members retained unchanged.
    pub elites: u32,
}

/// What a session has done on the device, for measurement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionStats {
    /// Device-local bytes allocated for the images, state and tables.
    pub device_bytes: u64,
    /// Host-visible bytes allocated for staging and readback.
    pub host_bytes: u64,
    /// Bytes moved from the host to the device, the images included.
    pub uploaded: u64,
    /// Bytes moved from the device to the host.
    pub read_back: u64,
    /// Time spent uploading the images and parameters.
    pub upload_time: Duration,
}

/// A candidate after evaluation on the device: its program with any repair appended, and what the
/// device decided about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evaluation {
    pub program: Vec<Op>,
    /// The raw payload size, or `None` when the candidate was refused.
    pub size: Option<u64>,
    /// The shader status code. `S_OK` when the candidate has a size.
    pub status: u32,
    /// Data bytes the program carries, repair included.
    pub blob_len: u64,
    /// Mismatch spans the repair covers, and the bytes in them.
    pub mismatch_spans: u32,
    pub mismatch_bytes: u32,
}

/// A program the search kept, with the raw payload size the device scored it at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finalist {
    pub program: Vec<Op>,
    /// The size of the payload the program makes with every data operation stored raw.
    pub raw_size: u64,
}

#[derive(Clone, Copy, Debug)]
struct Tables {
    stride: usize,
    old_entries: usize,
    new_entries: usize,
    old_cap: usize,
    new_cap: usize,
    old_table: usize,
    new_table: usize,
    classes: usize,
    changed: usize,
    blocks: usize,
    bit_words: usize,
    eq: usize,
    zero: usize,
    cells: usize,
    words: usize,
}

fn table_shape(old_len: usize, new_len: usize) -> (usize, usize, usize) {
    let entries_at = |len: usize, stride: usize| {
        if len < PROBE {
            0
        } else {
            (len - PROBE) / stride + 1
        }
    };
    let mut stride = IDX_MIN_STRIDE;
    while std::cmp::max(entries_at(old_len, stride), entries_at(new_len, stride)) > IDX_MAX_ENTRIES
    {
        stride *= 2;
    }
    return (
        stride,
        entries_at(old_len, stride),
        entries_at(new_len, stride),
    );
}

fn tables_for(old_len: usize, new_len: usize) -> Tables {
    let (stride, old_entries, new_entries) = table_shape(old_len, new_len);
    let cap = |n: usize| if n == 0 { 0 } else { n.next_power_of_two() };
    let (old_cap, new_cap) = (cap(old_entries), cap(new_entries));
    let cells = new_len.div_ceil(GRID);
    let blocks = cells.div_ceil(BLOCK_CELLS);
    let old_table = 0;
    let new_table = old_table + old_cap * 2;
    let classes = new_table + new_cap * 2;
    let changed = classes + cells;
    let block_words = changed + cells;
    let bit_words = new_len.div_ceil(32);
    let eq = block_words + blocks * 2 + 2;
    let zero = eq + bit_words;
    return Tables {
        stride: stride,
        old_entries: old_entries,
        new_entries: new_entries,
        old_cap: old_cap,
        new_cap: new_cap,
        old_table: old_table,
        new_table: new_table,
        classes: classes,
        changed: changed,
        blocks: block_words,
        bit_words: bit_words,
        eq: eq,
        zero: zero,
        cells: cells,
        words: zero + bit_words,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Which {
    Images,
    State,
    Index,
}

/// The device's whole search: the images, the population and the tables, resident until it is
/// dropped. See the module documentation.
pub struct Session {
    images: Buffer,
    state: Buffer,
    index: Buffer,
    _runs: Buffer,
    staging: Buffer,
    readback: Buffer,
    gpu: Gpu,
    config: SessionConfig,
    old_len: usize,
    new_len: usize,
    new_base: usize,
    scan_ranges: usize,
    tables: Tables,
    stats: SessionStats,
    next_generation: u32,
}

fn words_of(bytes: usize) -> usize {
    return bytes.div_ceil(4);
}

impl Session {
    /// Allocate the session's buffers on `gpu`, upload both images once, and write the search
    /// parameters. Refuses an image the device cannot hold before anything is allocated.
    pub fn open(
        gpu: Gpu,
        old: &[u8],
        new: &[u8],
        config: SessionConfig,
    ) -> Result<Session, PayloadError> {
        let started = Instant::now();
        if config.tournament == 0
            || config.immigrant_percent > 100
            || config.crossover_percent > 100
            || config.elites as usize > POP
        {
            return Err(err(
                Code::unsupported_field,
                "invalid GA parameters: tournament must be positive, percentages at most 100, and elites at most POP",
            ));
        }
        if old.len() >= MAX_IMAGE || new.len() >= MAX_IMAGE {
            return Err(err(
                Code::resource_limit,
                "an image is larger than the GPU search supports (it must be smaller than 1 GiB)",
            ));
        }
        let tables = tables_for(old.len(), new.len());
        let limit = gpu.limits().max_buffer_range;
        let old_words = words_of(old.len()) + 2;
        let new_words = words_of(new.len()) + 2;
        let needs = [
            ("the images", ((old_words + new_words) * 4) as u64),
            ("the search state", (STATE_WORDS * 4) as u64),
            ("the copy-source tables", (tables.words * 4) as u64),
            ("the replay runs", (RUN_VECS * 16) as u64),
        ];
        for (what, bytes) in needs {
            if bytes > limit {
                return Err(err(
                    Code::resource_limit,
                    match what {
                        "the images" => {
                            "the two images together exceed the device's maximum storage buffer size"
                        }
                        "the search state" => {
                            "the search state exceeds the device's maximum storage buffer size"
                        }
                        "the replay runs" => {
                            "the replay runs exceed the device's maximum storage buffer size"
                        }
                        _ => {
                            "the copy-source tables exceed the device's maximum storage buffer size"
                        }
                    },
                ));
            }
        }
        let dev = &gpu.context().device.0;
        let memory = &gpu.context().memory;
        let make = |bytes: usize, place: Place| Buffer::create(dev, memory, bytes, place);
        let images = make((old_words + new_words) * 4, Place::Device)?;
        let state = make(STATE_WORDS * 4, Place::Device)?;
        let index = make(tables.words * 4, Place::Device)?;
        let runs = make(RUN_VECS * 16, Place::Device)?;
        let staging_bytes = std::cmp::max(
            std::cmp::min(STAGING_BYTES, std::cmp::max(old.len(), new.len()) + 8),
            STAGING_MIN,
        );
        let staging = make(staging_bytes, Place::Host)?;
        let readback = make(READBACK_BYTES, Place::Host)?;
        let stats = SessionStats {
            device_bytes: (images.bytes + state.bytes + index.bytes + runs.bytes) as u64,
            host_bytes: (staging.bytes + readback.bytes) as u64,
            ..SessionStats::default()
        };

        let infos: Vec<[vk::DescriptorBufferInfo; 1]> = [&images, &state, &index, &runs]
            .iter()
            .map(|b| {
                [vk::DescriptorBufferInfo::default()
                    .buffer(b.handle())
                    .offset(0)
                    .range(vk::WHOLE_SIZE)]
            })
            .collect();
        let writes: Vec<vk::WriteDescriptorSet> = (0..BINDINGS as usize)
            .map(|k| {
                vk::WriteDescriptorSet::default()
                    .dst_set(gpu.context().set)
                    .dst_binding(k as u32)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&infos[k])
            })
            .collect();
        unsafe { dev.update_descriptor_sets(&writes, &[]) };

        let mut session = Session {
            images: images,
            state: state,
            index: index,
            _runs: runs,
            staging: staging,
            readback: readback,
            gpu: gpu,
            config: config,
            old_len: old.len(),
            new_len: new.len(),
            new_base: old_words,
            scan_ranges: 0,
            tables: tables,
            stats: stats,
            next_generation: 1,
        };
        let handles = [session.state.handle(), session.images.handle()];
        let sizes = [ST_SCAN * 4, session.images.bytes];
        session.gpu.submit(|b| {
            for k in 0..handles.len() {
                b.zero(handles[k], 0, sizes[k]);
            }
            return Ok(());
        })?;
        session.upload(Which::Images, 0, old)?;
        session.upload(Which::Images, old_words * 4, new)?;
        session.upload_words(Which::State, ST_PARAMS, &session.params())?;
        session.scan_ranges = session.params()[P_SCAN_RANGES] as usize;
        let words = session.tables.bit_words;
        let groups = session.stride_groups(words);
        session
            .gpu
            .submit(|b| b.dispatch(Stage::Bitmaps, [0; PUSH_WORDS], groups, 1))?;
        session.stats.upload_time = started.elapsed();
        return Ok(session);
    }

    fn params(&self) -> [u32; PARAMS_WORDS] {
        let c = &self.config;
        let t = &self.tables;
        let mut p = [0u32; PARAMS_WORDS];
        p[P_OLD_LEN] = self.old_len as u32;
        p[P_NEW_LEN] = self.new_len as u32;
        p[P_SEED0] = c.seed as u32;
        p[P_SEED1] = (c.seed >> 32) as u32;
        p[P_MAX_OPS] = c.max_operations;
        p[P_MAX_EXT_OP] = c.max_extents_per_op;
        p[P_MAX_EXT_TOTAL] = c.max_total_extents;
        p[P_TOURNAMENT] = c.tournament;
        p[P_IMMIGRANT_PERCENT] = c.immigrant_percent;
        p[P_CROSSOVER_PERCENT] = c.crossover_percent;
        p[P_ELITES] = c.elites;
        p[P_READABLE] = c.readable_target as u32;
        p[P_NEW_BASE] = self.new_base as u32;
        p[P_SIG2] = c.model.signatures;
        p[P_PART_FIXED] = c.model.partition_fixed;
        p[P_MAN_FIXED] = c.model.manifest_fixed;
        p[P_MAX_MANIFEST] = c.model.max_manifest;
        p[P_DEC_SLOT] = c.model.decode_slots;
        p[P_DEC_HEAP] = c.model.decode_heap;
        p[P_DEC_TRANS] = c.model.decode_transient;
        p[P_MAX_DECODED] = c.model.max_decoded;
        let ranges = std::cmp::min(
            std::cmp::max(self.new_len.div_ceil(SCAN_BYTES), 1),
            SCAN_MAX,
        );
        p[P_SCAN_RANGES] = ranges as u32;
        p[P_SCAN_LEN] = self.new_len.div_ceil(ranges) as u32;
        p[P_NCELLS] = t.cells as u32;
        p[P_IDX_STRIDE] = t.stride as u32;
        p[P_OLD_ENTRIES] = t.old_entries as u32;
        p[P_NEW_ENTRIES] = t.new_entries as u32;
        p[P_OLD_CAP] = t.old_cap as u32;
        p[P_NEW_CAP] = t.new_cap as u32;
        p[P_OLD_TABLE] = t.old_table as u32;
        p[P_NEW_TABLE] = t.new_table as u32;
        p[P_CLASSES] = t.classes as u32;
        p[P_CHANGED] = t.changed as u32;
        p[P_BLOCKS] = t.blocks as u32;
        p[P_EQ_BITS] = t.eq as u32;
        p[P_ZERO_BITS] = t.zero as u32;
        p[P_HEADER] = delta_update_payload::layout::HEADER_SIZE as u32;
        return p;
    }

    /// The name of the device the session runs on.
    pub fn device_name(&self) -> &str {
        return self.gpu.device_name();
    }

    /// What the session has moved and allocated so far.
    pub fn stats(&self) -> SessionStats {
        return self.stats;
    }

    fn buffer(&self, which: Which) -> &Buffer {
        return match which {
            Which::Images => &self.images,
            Which::State => &self.state,
            Which::Index => &self.index,
        };
    }

    fn upload(&mut self, which: Which, at: usize, data: &[u8]) -> Result<(), PayloadError> {
        let dst = self.buffer(which).handle();
        let staging = self.staging.handle();
        let piece = self.staging.bytes;
        let mut done = 0usize;
        while done < data.len() {
            let n = std::cmp::min(piece, data.len() - done);
            let padded = (n + 3) & !3;
            unsafe {
                ptr::copy_nonoverlapping(data[done..].as_ptr(), self.staging.ptr, n);
                ptr::write_bytes(self.staging.ptr.add(n), 0, padded - n);
            }
            let offset = at + done;
            self.gpu.submit(|b| {
                b.copy(staging, 0, dst, offset, padded);
                return Ok(());
            })?;
            done += n;
        }
        self.stats.uploaded += data.len() as u64;
        return Ok(());
    }

    fn upload_words(&mut self, which: Which, at: usize, words: &[u32]) -> Result<(), PayloadError> {
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_ne_bytes()).collect();
        return self.upload(which, at * 4, &bytes);
    }

    fn upload_regions(
        &mut self,
        which: Which,
        regions: &[(usize, &[u32])],
    ) -> Result<(), PayloadError> {
        let dst = self.buffer(which).handle();
        let staging = self.staging.handle();
        let capacity = self.staging.bytes / 4;
        let mut pending: Vec<(usize, usize, usize)> = Vec::new();
        let mut used = 0usize;
        let mut moved = 0usize;
        for (at, data) in regions {
            let mut done = 0usize;
            while done < data.len() {
                if used == capacity {
                    self.flush_upload(staging, dst, &mut pending)?;
                    used = 0;
                }
                let n = std::cmp::min(capacity - used, data.len() - done);
                unsafe {
                    ptr::copy_nonoverlapping(
                        data[done..].as_ptr(),
                        (self.staging.ptr as *mut u32).add(used),
                        n,
                    );
                }
                pending.push((used, at + done, n));
                used += n;
                done += n;
                moved += n;
            }
        }
        self.flush_upload(staging, dst, &mut pending)?;
        self.stats.uploaded += (moved * 4) as u64;
        return Ok(());
    }

    fn flush_upload(
        &self,
        staging: vk::Buffer,
        dst: vk::Buffer,
        pending: &mut Vec<(usize, usize, usize)>,
    ) -> Result<(), PayloadError> {
        if pending.is_empty() {
            return Ok(());
        }
        self.gpu.submit(|b| {
            for (from, to, n) in pending.iter() {
                b.copy(staging, from * 4, dst, to * 4, n * 4);
            }
            return Ok(());
        })?;
        pending.clear();
        return Ok(());
    }

    fn download_regions(
        &mut self,
        which: Which,
        regions: &[(usize, usize)],
    ) -> Result<Vec<Vec<u32>>, PayloadError> {
        let src = self.buffer(which).handle();
        let readback = self.readback.handle();
        let capacity = self.readback.bytes / 4;
        let mut out: Vec<Vec<u32>> = regions
            .iter()
            .map(|(_, n)| Vec::with_capacity(*n))
            .collect();
        let mut pending: Vec<(usize, usize, usize, usize)> = Vec::new();
        let mut used = 0usize;
        let mut moved = 0usize;
        for (k, (at, count)) in regions.iter().enumerate() {
            let mut done = 0usize;
            while done < *count {
                if used == capacity {
                    self.flush_download(src, readback, &mut pending, &mut out)?;
                    used = 0;
                }
                let n = std::cmp::min(capacity - used, count - done);
                pending.push((k, at + done, used, n));
                used += n;
                done += n;
                moved += n;
            }
        }
        self.flush_download(src, readback, &mut pending, &mut out)?;
        self.stats.read_back += (moved * 4) as u64;
        return Ok(out);
    }

    fn flush_download(
        &self,
        src: vk::Buffer,
        readback: vk::Buffer,
        pending: &mut Vec<(usize, usize, usize, usize)>,
        out: &mut Vec<Vec<u32>>,
    ) -> Result<(), PayloadError> {
        if pending.is_empty() {
            return Ok(());
        }
        self.gpu.submit(|b| {
            for (_, from, to, n) in pending.iter() {
                b.copy(src, from * 4, readback, to * 4, n * 4);
            }
            return Ok(());
        })?;
        for (k, _, at, n) in pending.iter() {
            let words = unsafe {
                std::slice::from_raw_parts((self.readback.ptr as *const u32).add(*at), *n)
            };
            out[*k].extend_from_slice(words);
        }
        pending.clear();
        return Ok(());
    }

    fn download(
        &mut self,
        which: Which,
        at: usize,
        count: usize,
    ) -> Result<Vec<u32>, PayloadError> {
        let mut parts = self.download_regions(which, &[(at, count)])?;
        return Ok(parts.pop().unwrap_or_default());
    }

    fn write_slots(&mut self, first: usize, images: &[SlotImage]) -> Result<(), PayloadError> {
        let mut heads: Vec<Vec<u32>> = Vec::new();
        for image in images {
            let mut head = image.header.clone();
            head.extend_from_slice(&image.ops);
            heads.push(head);
        }
        let mut regions: Vec<(usize, &[u32])> = Vec::new();
        for (k, image) in images.iter().enumerate() {
            let base = ST_SLOTS + (first + k) * SLOT_WORDS;
            regions.push((base, &heads[k]));
            if !image.exts.is_empty() {
                regions.push((base + SLOT_EXT, &image.exts));
            }
        }
        return self.upload_regions(Which::State, &regions);
    }

    fn read_slots(&mut self, ids: &[u32]) -> Result<Vec<SlotImage>, PayloadError> {
        let base = |id: u32| ST_SLOTS + id as usize * SLOT_WORDS;
        let heads = self.download_regions(
            Which::State,
            &ids.iter()
                .map(|id| (base(*id), SLOT_HDR))
                .collect::<Vec<_>>(),
        )?;
        let mut regions: Vec<(usize, usize)> = Vec::new();
        for (k, id) in ids.iter().enumerate() {
            let nops = std::cmp::min(heads[k][H_NOPS] as usize, OP_CAP);
            let next = std::cmp::min(heads[k][H_NEXT] as usize, EXT_CAP);
            regions.push((base(*id) + SLOT_OPS, nops * OPW));
            regions.push((base(*id) + SLOT_EXT, next * 2));
        }
        let mut parts = self.download_regions(Which::State, &regions)?.into_iter();
        let mut out: Vec<SlotImage> = Vec::new();
        for header in heads {
            let ops = parts.next().unwrap_or_default();
            let exts = parts.next().unwrap_or_default();
            out.push(SlotImage {
                header: header,
                ops: ops,
                exts: exts,
            });
        }
        return Ok(out);
    }

    /// The control counters the shaders keep.
    pub fn counters(&mut self) -> Result<Counters, PayloadError> {
        let words = self.download(Which::State, ST_CTL, CTL_WORDS)?;
        return Ok(Counters::from_words(&words));
    }

    /// Evaluate up to [`NCAND`] programs as candidates, in order, through the same stages the
    /// search uses: replay on the memory model, mismatch scan and repair, and the exact size. Each
    /// result carries the program as stored, repair included. This is the differential-testing and
    /// diagnostic entry; the search itself never moves a chromosome across to the host.
    ///
    /// This diagnostic replaces candidate slots and clears the control counters, including mutation
    /// statistics; it is not a continuation of a search session. A program the chromosome cannot
    /// hold is refused here with the capacity status, without a device round trip.
    pub fn evaluate(&mut self, programs: &[Vec<Op>]) -> Result<Vec<Evaluation>, PayloadError> {
        if programs.len() > NCAND {
            return Err(err(
                Code::unsupported_field,
                "more programs than one evaluation pass holds",
            ));
        }
        let encoded: Vec<Option<SlotImage>> =
            programs.iter().map(|p| encode_program(p).ok()).collect();
        let images: Vec<SlotImage> = encoded
            .iter()
            .map(|e| e.clone().unwrap_or_else(SlotImage::skipped))
            .collect();
        self.write_slots(0, &images)?;
        let ids: Vec<u32> = (0..programs.len() as u32).collect();
        self.upload_words(Which::State, ST_CAND, &ids)?;
        self.upload_words(Which::State, ST_CTL, &[0u32; CTL_WORDS])?;
        let n = programs.len() as u32;
        self.gpu.submit(|b| self.record_evaluation(b, n))?;
        let stored = self.read_slots(&ids)?;

        let mut out: Vec<Evaluation> = Vec::new();
        for (j, program) in programs.iter().enumerate() {
            if encoded[j].is_none() {
                let capacity = if program.len() > OP_CAP {
                    S_OPCAP
                } else {
                    S_EXTCAP
                };
                out.push(Evaluation {
                    program: program.clone(),
                    size: None,
                    status: capacity as u32,
                    blob_len: 0,
                    mismatch_spans: 0,
                    mismatch_bytes: 0,
                });
                continue;
            }
            let slot = &stored[j];
            let status = slot.word(H_STATUS);
            let program = match decode_program(slot) {
                None => {
                    return Err(err(
                        Code::device_io,
                        "the device returned a malformed program",
                    ));
                }
                Some(p) => p,
            };
            out.push(Evaluation {
                program: program,
                size: if status == S_OK as u32 {
                    Some(slot.word(H_FIT) as u64)
                } else {
                    None
                },
                status: status,
                blob_len: slot.word(H_BLOB) as u64,
                mismatch_spans: slot.word(H_MISM),
                mismatch_bytes: slot.word(H_MISMBYTES),
            });
        }
        return Ok(out);
    }

    /// Replace the population with `programs`, evaluated and selected as a seeding generation would be:
    /// those that are admitted become the members, in order of raw size, and the finalist archive
    /// restarts from them. A diagnostic and test entry, for breeding from a known parent. The session
    /// must have been prepared, since the tables and the classification come from there.
    pub fn load_population(&mut self, programs: &[Vec<Op>]) -> Result<Counters, PayloadError> {
        if programs.len() > NCAND {
            return Err(err(
                Code::unsupported_field,
                "more programs than one population holds",
            ));
        }
        self.gpu
            .submit(|b| b.dispatch(Stage::InitState, [0; PUSH_WORDS], 1, 1))?;
        let images: Vec<SlotImage> = programs
            .iter()
            .map(|p| encode_program(p).unwrap_or_else(|_| SlotImage::skipped()))
            .collect();
        self.write_slots(0, &images)?;
        let n = programs.len() as u32;
        self.gpu.submit(|b| {
            self.record_evaluation(b, n)?;
            return b.dispatch(Stage::Select, [0, n, 0, 0, 0], 1, 1);
        })?;
        self.next_generation = 1;
        return self.counters();
    }

    /// Re-seed the population with the archive and deterministic immigrants after a plateau.
    /// The archive is retained as the set of elites, while immigrants are generated using the
    /// current global generation so consecutive restarts do not repeat the same proposals.
    pub fn restart_from_archive(&mut self) -> Result<Counters, PayloadError> {
        let previous = self.counters()?;
        let previous_ctl = self.download(Which::State, ST_CTL, CTL_WORDS)?;
        let generation = self.next_generation;
        let archive = self.finalists()?;
        let proposals = self.breed_only(generation, 1, 0)?;
        let mut programs: Vec<Vec<Op>> = archive.into_iter().map(|f| f.program).collect();
        for program in proposals.into_iter().flatten() {
            if programs.len() >= NCAND {
                break;
            }
            programs.push(program);
        }
        let current = self.load_population(&programs)?;
        self.next_generation = generation;

        let reset_ctl = self.download(Which::State, ST_CTL, CTL_WORDS)?;
        let mut ctl: [u32; CTL_WORDS] = previous_ctl
            .try_into()
            .unwrap_or_else(|_| [0u32; CTL_WORDS]);
        ctl[C_GEN] = previous.generations;
        ctl[C_BEST] = std::cmp::min(previous.best, current.best);
        ctl[C_NVALID] = current.members;
        ctl[C_EVALS] = previous.evaluated.saturating_add(programs.len() as u32);
        ctl[C_ARCHIVED] = current.archived;
        ctl[C_BRED] = previous.bred.saturating_add(CHILDREN as u32);
        ctl[C_SKIPPED] = previous.skipped;
        for status in 0..S_COUNT {
            ctl[C_REJ + status] = ctl[C_REJ + status].saturating_add(reset_ctl[C_REJ + status]);
        }
        self.upload_words(Which::State, ST_CTL, &ctl)?;
        return self.counters();
    }

    /// Breed one generation of children from the current population and return them as bred, before
    /// any evaluation: `None` for a child that was an unchanged parent and is skipped. `how` forces
    /// how children are made (0 as the search does, 1 immigrants only, 2 crossover only, 3 mutation
    /// only), and `mv` forces which move a mutation makes, as a [`MV_SPLIT`]-style code plus one, or
    /// 0 for the search's own weighted choice. Nothing is evaluated or selected, so a test can see
    /// what one operator proposes. The population must have been seeded by [`prepare`](Self::prepare).
    pub fn breed_only(
        &mut self,
        generation: u32,
        how: u32,
        mv: u32,
    ) -> Result<Vec<Option<Vec<Op>>>, PayloadError> {
        self.gpu.submit(|b| {
            b.dispatch(
                Stage::Breed,
                [generation, mv, how, 0, 0],
                CHILDREN as u32,
                1,
            )
        })?;
        let ids = self.download(Which::State, ST_CAND, CHILDREN)?;
        let slots = self.read_slots(&ids)?;
        let mut out: Vec<Option<Vec<Op>>> = Vec::new();
        for slot in slots {
            if slot.word(H_STATUS) as usize == S_SKIP {
                out.push(None);
                continue;
            }
            out.push(decode_program(&slot));
        }
        return Ok(out);
    }

    /// Run and select one forced generation, using the same `how` and `mv` codes as
    /// [`breed_only`](Self::breed_only). Mutation counters are recorded at selection, not breeding.
    /// Invalid force codes are refused; a crossover with fewer than two members falls back to a
    /// mutation, and an empty population always produces immigrants.
    pub fn run_forced_generation(&mut self, how: u32, mv: u32) -> Result<Counters, PayloadError> {
        if how > 3 || mv > MV_COUNT as u32 {
            return Err(err(
                Code::unsupported_field,
                "invalid forced breeding mode or mutation",
            ));
        }
        let generation = self.next_generation;
        let n = CHILDREN as u32;
        self.gpu.submit(|b| {
            b.dispatch(Stage::Breed, [generation, mv, how, 0, 0], n, 1)?;
            self.record_evaluation_in(b, generation, n)?;
            return b.dispatch(Stage::Select, [generation, n, 0, 0, 0], 1, 1);
        })?;
        self.next_generation = generation + 1;
        return self.counters();
    }

    /// The programs the population holds, best first, with the raw size each was scored at.
    pub fn population(&mut self) -> Result<Vec<Finalist>, PayloadError> {
        let members = self.counters()?.members as usize;
        let ids = self.download(Which::State, ST_POP, members)?;
        let slots = self.read_slots(&ids)?;
        let mut out: Vec<Finalist> = Vec::new();
        for slot in slots {
            let program = match decode_program(&slot) {
                None => {
                    return Err(err(
                        Code::device_io,
                        "the device returned a malformed population member",
                    ));
                }
                Some(p) => p,
            };
            out.push(Finalist {
                program: program,
                raw_size: slot.word(H_FIT) as u64,
            });
        }
        return Ok(out);
    }

    /// The classification of each [`GRID`]-byte cell of the new image, as the device computed it:
    /// bit 0 ([`CELL_EQUAL`]) when the old image holds exactly the same bytes, bit 1 ([`CELL_ZERO`])
    /// when the new bytes are all zero. Valid after [`prepare`](Self::prepare).
    pub fn classes(&mut self) -> Result<Vec<u8>, PayloadError> {
        let t = self.tables;
        let words = self.download(Which::Index, t.classes, t.cells)?;
        return Ok(words.into_iter().map(|w| w as u8).collect());
    }

    /// The indices of the cells that differ from the old image, in ascending order, as the device
    /// listed them. Valid after [`prepare`](Self::prepare).
    pub fn changed_cells(&mut self) -> Result<Vec<u32>, PayloadError> {
        let t = self.tables;
        let count = self.download(Which::State, ST_PARAMS + P_NCHANGED, 1)?[0] as usize;
        return self.download(Which::Index, t.changed, std::cmp::min(count, t.cells));
    }

    /// `count` words of the state buffer from word `at`, for diagnosis.
    pub fn read_state_words(&mut self, at: usize, count: usize) -> Result<Vec<u32>, PayloadError> {
        return self.download(Which::State, at, count);
    }

    fn stride_groups(&self, items: usize) -> u32 {
        let wanted = items.div_ceil(WORKGROUP).max(1);
        let cap = std::cmp::min(self.gpu.limits().max_groups_x as usize, 4096);
        return std::cmp::min(wanted, cap) as u32;
    }

    /// Build everything the search needs before its first generation: the sorted copy-source tables
    /// of both images, the classification of the new image, the list of changed cells, and the
    /// first population (the raw replacement, the compact program, the repair of the empty program,
    /// and random immigrants), evaluated and selected. Returns the counters.
    pub fn prepare(&mut self) -> Result<Counters, PayloadError> {
        let t = self.tables;
        let cells = t.cells;
        let blocks = cells.div_ceil(BLOCK_CELLS);
        self.gpu.submit(|b| {
            for (which, cap) in [(0u32, t.old_cap), (1u32, t.new_cap)] {
                if cap == 0 {
                    continue;
                }
                b.dispatch(
                    Stage::IdxFill,
                    [0, which, 0, 0, 0],
                    self.stride_groups(cap),
                    1,
                )?;
                let mut k = 2usize;
                while k <= cap {
                    let mut j = k / 2;
                    while j > 0 {
                        b.dispatch(
                            Stage::IdxSort,
                            [0, which, k as u32, j as u32, 0],
                            self.stride_groups(cap / 2),
                            1,
                        )?;
                        j /= 2;
                    }
                    k *= 2;
                }
            }
            b.dispatch(
                Stage::Classify,
                [0; PUSH_WORDS],
                self.stride_groups(cells),
                1,
            )?;
            b.dispatch(
                Stage::CellsCount,
                [0; PUSH_WORDS],
                self.stride_groups(blocks),
                1,
            )?;
            b.dispatch(Stage::CellsScan, [0; PUSH_WORDS], 1, 1)?;
            b.dispatch(
                Stage::CellsEmit,
                [0; PUSH_WORDS],
                self.stride_groups(blocks),
                1,
            )?;
            b.dispatch(Stage::InitState, [0; PUSH_WORDS], 1, 1)?;
            b.dispatch(Stage::Seed, [0; PUSH_WORDS], NCAND as u32, 1)?;
            self.record_evaluation(b, NCAND as u32)?;
            b.dispatch(Stage::Select, [0, NCAND as u32, 0, 0, 0], 1, 1)?;
            return Ok(());
        })?;
        self.next_generation = 1;
        return self.counters();
    }

    /// Run one generation a stage at a time and return the host-measured time of each, in order. It is
    /// the same generation `run_generations` runs, with a submission and a wait per stage, so the times
    /// include that overhead (about half a millisecond on a hardware device). For diagnosis and
    /// benchmarks; the search never does this.
    pub fn profile_generation(&mut self) -> Result<Vec<(&'static str, Duration)>, PayloadError> {
        let generation = self.next_generation;
        let n = CHILDREN as u32;
        let range_groups = self.scan_ranges.div_ceil(WORKGROUP) as u32;
        let steps: [(&'static str, Stage, u32, u32, u32); 7] = [
            ("breed", Stage::Breed, 0, n, 1),
            ("replay", Stage::EvalReplay, n, n, 1),
            ("count", Stage::EvalCount, n, range_groups, n),
            ("prefix", Stage::EvalPrefix, n, n, 1),
            ("emit", Stage::EvalEmit, n, range_groups, n),
            ("finish", Stage::EvalFinish, n, n, 1),
            ("select", Stage::Select, n, 1, 1),
        ];
        let mut times: Vec<(&'static str, Duration)> = Vec::new();
        for (name, stage, count, groups_x, groups_y) in steps {
            let started = Instant::now();
            self.gpu
                .submit(|b| b.dispatch(stage, [generation, count, 0, 0, 0], groups_x, groups_y))?;
            times.push((name, started.elapsed()));
        }
        self.next_generation = generation + 1;
        return Ok(times);
    }

    /// The generations run so far after seeding: the number the next generation will carry.
    pub fn next_generation(&self) -> u32 {
        return self.next_generation;
    }

    /// Run `count` generations in one submission and return the counters afterwards. Each is breed,
    /// evaluate and select, with a memory barrier between every stage. The host reads only the
    /// counters, so the cost of a batch is the device's, and its length is the caller's to choose.
    /// The result does not depend on how generations are split into batches.
    pub fn run_generations(&mut self, count: u32) -> Result<Counters, PayloadError> {
        let first = self.next_generation;
        self.gpu.submit(|b| {
            for g in 0..count {
                let generation = first + g;
                b.dispatch(Stage::Breed, [generation, 0, 0, 0, 0], CHILDREN as u32, 1)?;
                self.record_evaluation_in(b, generation, CHILDREN as u32)?;
                b.dispatch(Stage::Select, [generation, CHILDREN as u32, 0, 0, 0], 1, 1)?;
            }
            return Ok(());
        })?;
        self.next_generation = first + count;
        return self.counters();
    }

    /// The archive: the best programs the search admitted, one per distinct set of raw-written
    /// bytes, best first. Read back once, when the search ends.
    pub fn finalists(&mut self) -> Result<Vec<Finalist>, PayloadError> {
        let counters = self.counters()?;
        let ids: Vec<u32> = (0..counters.archived)
            .map(|a| (SLOT_ARCH + a as usize) as u32)
            .collect();
        let mut out: Vec<Finalist> = Vec::new();
        for slot in self.read_slots(&ids)? {
            if slot.word(H_FIT) == FITNESS_INVALID {
                continue;
            }
            let program = match decode_program(&slot) {
                None => {
                    return Err(err(
                        Code::device_io,
                        "the device returned a malformed finalist",
                    ));
                }
                Some(p) => p,
            };
            out.push(Finalist {
                program: program,
                raw_size: slot.word(H_FIT) as u64,
            });
        }
        out.sort_by(|a, b| a.raw_size.cmp(&b.raw_size));
        return Ok(out);
    }

    fn record_evaluation(&self, b: &Batch, n: u32) -> Result<(), PayloadError> {
        return self.record_evaluation_in(b, 0, n);
    }

    fn record_evaluation_in(&self, b: &Batch, generation: u32, n: u32) -> Result<(), PayloadError> {
        let range_groups = self.scan_ranges.div_ceil(WORKGROUP) as u32;
        b.dispatch(Stage::EvalReplay, [generation, n, 0, 0, 0], n, 1)?;
        b.dispatch(Stage::EvalCount, [generation, n, 0, 0, 0], range_groups, n)?;
        b.dispatch(Stage::EvalPrefix, [generation, n, 0, 0, 0], n, 1)?;
        b.dispatch(Stage::EvalEmit, [generation, n, 0, 0, 0], range_groups, n)?;
        b.dispatch(Stage::EvalFinish, [generation, n, 0, 0, 0], n, 1)?;
        return Ok(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(name: &str, kind: Kind) -> Candidate {
        return Candidate {
            physical: vk::PhysicalDevice::null(),
            family: 0,
            name: name.to_string(),
            kind: kind,
            limits: DeviceLimits {
                max_buffer_range: 1 << 27,
                max_groups_x: 65535,
                max_groups_y: 65535,
            },
        };
    }

    fn open_software() -> Gpu {
        return match Gpu::open(DevicePreference::Software) {
            Ok(g) => g,
            Err(e) => panic!("{}", e.msg),
        };
    }

    #[test]
    fn move_statistics_saturate_and_credit_only_the_deterministic_best() {
        let old = vec![1u8; 64];
        let new = vec![2u8; 64];
        let problem = crate::evaluate::Problem::new(
            &old,
            &new,
            64,
            "A/B",
            &delta_update_payload::profile::HOST,
            256,
        )
        .unwrap();
        let config =
            crate::search::session_config(&problem, &crate::search::SearchConfig::quick()).unwrap();
        let mut session = Session::open(open_software(), &old, &new, config).unwrap();
        session.prepare().unwrap();
        session
            .gpu
            .submit(|b| b.dispatch(Stage::InitState, [0; PUSH_WORDS], 1, 1))
            .unwrap();
        let mut first = encode_program(&[]).unwrap();
        first.header[H_FIT] = 50;
        first.header[H_MOVE] = 1;
        let mut tied = first.clone();
        tied.header[H_MOVE] = 2;
        let mut skipped = SlotImage::skipped();
        skipped.header[H_MOVE] = 1;
        session.write_slots(0, &[first, tied, skipped]).unwrap();
        session
            .upload_words(Which::State, ST_CTL + C_BEST, &[100])
            .unwrap();
        session
            .upload_words(Which::State, ST_CTL + C_MOVE, &[u32::MAX - 1; MOVE_STATS])
            .unwrap();
        session
            .gpu
            .submit(|b| b.dispatch(Stage::Select, [0, 3, 0, 0, 0], 1, 1))
            .unwrap();
        let stats = session.counters().unwrap().moves;
        assert_eq!(
            stats[0],
            MoveCounters {
                proposed: u32::MAX,
                skipped: u32::MAX,
                admitted: u32::MAX,
                new_best: u32::MAX,
            }
        );
        assert_eq!(
            stats[1],
            MoveCounters {
                proposed: 1,
                admitted: 1,
                ..MoveCounters::default()
            }
        );
    }

    #[test]
    fn software_device_is_the_cpu_type_lavapipe_device() {
        let gpu = open_software();
        assert!(
            gpu.device_name().contains("llvmpipe"),
            "got {}",
            gpu.device_name()
        );
    }

    #[test]
    fn concurrent_opens_and_drops_all_succeed() {
        let threads = 6;
        let rounds = 2;
        let start = std::sync::Arc::new(std::sync::Barrier::new(threads));
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    for _ in 0..rounds {
                        match Gpu::open(DevicePreference::Software) {
                            Ok(g) => drop(g),
                            Err(e) => return Some(e.msg),
                        }
                    }
                    return None;
                })
            })
            .collect();
        for w in workers {
            let outcome = w.join().expect("an opening thread panicked");
            assert_eq!(outcome, None, "a concurrent open failed");
        }
    }

    #[test]
    fn hardware_candidates_come_before_the_software_device() {
        let found = vec![
            candidate("cpu", Kind::Software),
            candidate("igpu", Kind::Integrated),
            candidate("dgpu", Kind::Discrete),
        ];
        let names: Vec<String> = ordered(found).into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["dgpu", "igpu", "cpu"]);
    }

    #[test]
    fn a_failed_hardware_attempt_falls_back_to_the_software_device() {
        let found = ordered(vec![
            candidate("cpu", Kind::Software),
            candidate("igpu", Kind::Integrated),
            candidate("dgpu", Kind::Discrete),
        ]);
        let outcome = first_working(&found, |c| {
            if c.kind == Kind::Software {
                Ok(c.name.clone())
            } else {
                Err(err(Code::device_io, "driver refused"))
            }
        });
        let (chosen, failed) = match outcome {
            Ok(v) => v,
            Err(f) => panic!("{}", f.error.msg),
        };
        assert_eq!(chosen, "cpu");
        let tried: Vec<&str> = failed.iter().map(|a| a.device.as_str()).collect();
        assert_eq!(
            tried,
            vec!["dgpu", "igpu"],
            "each hardware failure is recorded"
        );
    }

    #[test]
    fn when_every_device_fails_the_failure_lists_each_attempt() {
        let found = ordered(vec![
            candidate("igpu", Kind::Integrated),
            candidate("cpu", Kind::Software),
        ]);
        let outcome: Result<((), Vec<Attempt>), DeviceFailure> = first_working(&found, |c| {
            let message = if c.kind == Kind::Software {
                "cpu broken"
            } else {
                "igpu broken"
            };
            Err(err(Code::device_io, message))
        });
        let failure = match outcome {
            Ok(_) => panic!("no device works, so the open must fail"),
            Err(f) => f,
        };
        assert_eq!(failure.error.code, Code::no_compute_device);
        let names: Vec<&str> = failure.attempts.iter().map(|a| a.device.as_str()).collect();
        assert_eq!(names, vec!["igpu", "cpu"]);
    }

    #[test]
    fn device_memory_is_preferred_over_host_visible_memory_for_the_search_state() {
        let mut props = vk::PhysicalDeviceMemoryProperties::default();
        props.memory_type_count = 3;
        props.memory_types[0].property_flags =
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        props.memory_types[1].property_flags = vk::MemoryPropertyFlags::DEVICE_LOCAL
            | vk::MemoryPropertyFlags::HOST_VISIBLE
            | vk::MemoryPropertyFlags::HOST_COHERENT;
        props.memory_types[2].property_flags = vk::MemoryPropertyFlags::DEVICE_LOCAL;
        assert_eq!(
            memory_type(&props, u32::MAX, Place::Device),
            Some(2),
            "pure VRAM wins"
        );
        assert_eq!(memory_type(&props, u32::MAX, Place::Host), Some(0));
        assert_eq!(memory_type(&props, 0b011, Place::Device), Some(1));
        props.memory_types[1].property_flags =
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        props.memory_types[2].property_flags = vk::MemoryPropertyFlags::empty();
        assert_eq!(memory_type(&props, u32::MAX, Place::Device), Some(0));
    }

    #[test]
    fn table_shapes_grow_their_stride_until_they_fit_the_cap() {
        assert_eq!(table_shape(0, 0), (IDX_MIN_STRIDE, 0, 0));
        assert_eq!(table_shape(PROBE - 1, PROBE), (IDX_MIN_STRIDE, 0, 1));
        let (stride, old, new) = table_shape(1 << 20, 1 << 20);
        assert_eq!(stride, IDX_MIN_STRIDE);
        assert_eq!(old, ((1 << 20) - PROBE) / IDX_MIN_STRIDE + 1);
        assert_eq!(old, new);
        let (stride, old, _) = table_shape(1 << 30, 1 << 10);
        assert!(stride > IDX_MIN_STRIDE && stride.is_power_of_two());
        assert!(old <= IDX_MAX_ENTRIES);
    }
}
