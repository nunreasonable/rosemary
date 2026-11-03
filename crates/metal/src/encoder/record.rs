//! `ROSEMARY_METAL_RECORD_THREAD=1`: record encoders on the `metal:record`
//! thread, so a game that encodes on its main thread spends less of each frame
//! here. An encoder call keeps what it needs from the guest and queues the same
//! call. Anything that submits or waits stays on the guest's thread and first
//! waits for the queue, so submissions keep their place and order.

use super::*;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::Once;

use static_init::dynamic;

type Job = Box<dyn FnOnce() + Send>;

pub(super) fn enabled() -> bool {
    #[dynamic]
    static ON: bool =
        std::env::var_os("ROSEMARY_METAL_RECORD_THREAD").is_some_and(|value| value != "0");

    *ON
}

/// Calls queued and calls made, counted so a wait can stop at the calls that
/// were queued when it began rather than at an empty queue another thread may
/// never leave empty.
struct Queue {
    jobs: VecDeque<Job>,
    queued: u64,
    done: u64,
    /// Whether the worker is waiting for work, the only time it needs waking.
    asleep: bool,
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue {
    jobs: VecDeque::new(),
    queued: 0,
    done: 0,
    asleep: false,
});
static WORK: Condvar = Condvar::new();
static IDLE: Condvar = Condvar::new();
static WORKER: Once = Once::new();

/// Calls a thread collects before it hands them over: one handover, and at
/// most one wake of the worker, per this many calls rather than per call.
const BATCH: usize = 64;

/// This thread's calls not yet handed over. A thread that ends with some hands
/// them over then, so none is lost.
struct Pending(Vec<Job>);

impl Drop for Pending {
    fn drop(&mut self) {
        hand_over(std::mem::take(&mut self.0));
    }
}

thread_local! {
    static ON_WORKER: Cell<bool> = const { Cell::new(false) };
    static PENDING: RefCell<Pending> = const { RefCell::new(Pending(Vec::new())) };
}

fn enqueue(job: Job) {
    // A queued call can release the last reference to an object whose
    // `-dealloc` queues or waits in turn. On the worker that call simply runs.
    if ON_WORKER.get() {
        job();
        return;
    }
    let full = PENDING.with_borrow_mut(|pending| {
        pending.0.push(job);
        pending.0.len() >= BATCH
    });
    if full {
        send();
    }
}

/// Hand this thread's calls to the worker now.
fn send() {
    let batch = PENDING.with_borrow_mut(|pending| std::mem::take(&mut pending.0));
    hand_over(batch);
}

fn hand_over(batch: Vec<Job>) {
    if batch.is_empty() {
        return;
    }
    WORKER.call_once(|| {
        // Detached, like this crate's other workers: it runs for the life of
        // the process.
        std::thread::Builder::new()
            .name("metal:record".to_string())
            .spawn(worker)
            .expect("a thread to record Metal encoders on");
    });
    let mut queue = QUEUE.lock().expect("the recording queue");
    queue.queued += batch.len() as u64;
    queue.jobs.extend(batch);
    let asleep = queue.asleep;
    drop(queue);
    if asleep {
        WORK.notify_one();
    }
}

/// Wait until every call queued so far has been made. Free when nothing is
/// queued, and free with the variable off.
pub(super) fn drain() {
    if !enabled() || ON_WORKER.get() {
        return;
    }
    send();
    let mut queue = QUEUE.lock().expect("the recording queue");
    let target = queue.queued;
    if queue.done >= target {
        return;
    }
    let start = stats::timer();
    while queue.done < target {
        queue = IDLE.wait(queue).expect("the recording queue");
    }
    drop(queue);
    stats::finish_timer(Counter::RecordDrainMicros, start);
}

fn worker() {
    ON_WORKER.set(true);
    // The calls are this crate's own, but releasing what they held can reach a
    // guest's `-dealloc`, which reads Darwin's thread-local state.
    let _darwin = darwin::tls::DarwinThread::enter();
    loop {
        let batch = {
            let mut queue = QUEUE.lock().expect("the recording queue");
            while queue.jobs.is_empty() {
                queue.asleep = true;
                queue = WORK.wait(queue).expect("the recording queue");
                queue.asleep = false;
            }
            std::mem::take(&mut queue.jobs)
        };
        let count = batch.len() as u64;
        let pool = objc::arc::pool_push();
        for job in batch {
            job();
        }
        objc::arc::pool_pop(pool);
        stats::add(Counter::RecordJobs, count);
        QUEUE.lock().expect("the recording queue").done += count;
        IDLE.notify_all();
    }
}

/// How an argument crosses to the worker: an object as a reference held until
/// the call has been made, anything else by value.
trait Carry: Sized {
    type Held: Send;
    fn hold(self) -> Self::Held;
    fn get(held: &Self::Held) -> Self;
}

impl Carry for Id {
    type Held = OwnedObjcRef;
    fn hold(self) -> OwnedObjcRef {
        OwnedObjcRef::retain(self)
    }
    fn get(held: &OwnedObjcRef) -> Id {
        held.as_id()
    }
}

macro_rules! by_value {
    ($($type:ty),*) => {$(
        impl Carry for $type {
            type Held = $type;
            fn hold(self) -> $type {
                self
            }
            fn get(held: &$type) -> $type {
                *held
            }
        }
    )*};
}

by_value!(
    u32,
    u64,
    i64,
    f32,
    f64,
    MtlSize,
    MtlOrigin,
    Viewport,
    ScissorRect
);

/// A queued wrapper per method: the same signature, the same function called
/// on the worker. `sel` goes as an address because some of them branch on it.
macro_rules! queued {
    ($($name:ident $(<$stage:ident>)? => $original:ident($($arg:ident: $type:ty),*);)*) => {$(
        unsafe extern "C" fn $name $(<const $stage: usize>)? (receiver: Id, sel: Sel, $($arg: $type),*) {
            let receiver = OwnedObjcRef::retain(receiver);
            let sel = sel as usize;
            $(let $arg = <$type as Carry>::hold($arg);)*
            enqueue(Box::new(move || {
                // SAFETY: the call the guest made, with every object it named
                // still held.
                unsafe {
                    $original $(::<$stage>)? (receiver.as_id(), sel as Sel, $(<$type as Carry>::get(&$arg)),*)
                }
            }));
        }
    )*};
}

/// A wrapper per method that touches more than the recording: wait for the
/// queue, then make the call as before, on the guest's thread.
macro_rules! drained {
    ($($name:ident => $original:ident($($arg:ident: $type:ty),*) $(-> $return:ty)?;)*) => {$(
        unsafe extern "C" fn $name(receiver: Id, sel: Sel, $($arg: $type),*) $(-> $return)? {
            drain();
            // SAFETY: the guest's call, unchanged.
            unsafe { $original(receiver, sel, $($arg),*) }
        }
    )*};
}

queued! {
    end_encoding_queued => end_encoding();
    set_compute_pipeline_q => set_compute_pipeline(pipeline: Id);
    set_render_pipeline_q => set_render_pipeline(pipeline: Id);
    set_depth_stencil_state_q => set_depth_stencil_state(state: Id);
    set_stencil_reference_q => set_stencil_reference(value: u32);
    set_stencil_references_q => set_stencil_references(front: u32, back: u32);
    set_viewport_q => set_viewport(viewport: Viewport);
    set_scissor_q => set_scissor(rect: ScissorRect);
    set_depth_bias_q => set_depth_bias(bias: f32, slope_scale: f32, clamp: f32);
    set_blend_color_q => set_blend_color(red: f32, green: f32, blue: f32, alpha: f32);
    set_raster_value_q => set_raster_value(value: u64);
    set_visibility_result_mode_q => set_visibility_result_mode(mode: u64, offset: u64);
    memory_barrier_q => memory_barrier();
    dispatch_threadgroups_q => dispatch_threadgroups(groups: MtlSize, per_group: MtlSize);
    draw_primitives_q => draw_primitives(
        primitive: u64, vertex_start: u64, vertex_count: u64, instance_count: u64);
    draw_primitives_base_q => draw_primitives_base(
        primitive: u64, vertex_start: u64, vertex_count: u64, instance_count: u64,
        base_instance: u64);
    draw_primitives_once_q => draw_primitives_once(
        primitive: u64, vertex_start: u64, vertex_count: u64);
    draw_indexed_primitives_q => draw_indexed_primitives(
        primitive: u64, index_count: u64, index_type: u64, index_buffer: Id,
        index_offset: u64, instance_count: u64);
    draw_indexed_primitives_base_q => draw_indexed_primitives_base(
        primitive: u64, index_count: u64, index_type: u64, index_buffer: Id,
        index_offset: u64, instance_count: u64, base_vertex: i64, base_instance: u64);
    draw_indexed_primitives_once_q => draw_indexed_primitives_once(
        primitive: u64, index_count: u64, index_type: u64, index_buffer: Id,
        index_offset: u64);
    draw_primitives_indirect_q => draw_primitives_indirect(
        primitive: u64, indirect: Id, offset: u64);
    draw_indexed_primitives_indirect_q => draw_indexed_primitives_indirect(
        primitive: u64, index_type: u64, index_buffer: Id, index_offset: u64,
        indirect: Id, offset: u64);
    copy_buffer_to_buffer_q => copy_buffer_to_buffer(
        source: Id, source_offset: u64, destination: Id, destination_offset: u64, size: u64);
    copy_buffer_to_texture_q => copy_buffer_to_texture(
        source: Id, source_offset: u64, bytes_per_row: u64, bytes_per_image: u64,
        size: MtlSize, destination: Id, slice: u64, level: u64, origin: MtlOrigin);
    copy_buffer_to_texture_options_q => copy_buffer_to_texture_options(
        source: Id, source_offset: u64, bytes_per_row: u64, bytes_per_image: u64,
        size: MtlSize, destination: Id, slice: u64, level: u64, origin: MtlOrigin,
        options: u64);
    copy_texture_to_buffer_q => copy_texture_to_buffer(
        source: Id, slice: u64, level: u64, origin: MtlOrigin, size: MtlSize,
        destination: Id, destination_offset: u64, bytes_per_row: u64, bytes_per_image: u64);
    copy_texture_to_buffer_options_q => copy_texture_to_buffer_options(
        source: Id, slice: u64, level: u64, origin: MtlOrigin, size: MtlSize,
        destination: Id, destination_offset: u64, bytes_per_row: u64, bytes_per_image: u64,
        options: u64);
    copy_texture_to_texture_q => copy_texture_to_texture(
        source: Id, source_slice: u64, source_level: u64, source_origin: MtlOrigin,
        size: MtlSize, destination: Id, destination_slice: u64, destination_level: u64,
        destination_origin: MtlOrigin);
    generate_mipmaps_q => generate_mipmaps(texture: Id);
    set_buffer_q<STAGE> => set_buffer(buffer: Id, offset: u64, index: u64);
    set_buffer_offset_q<STAGE> => set_buffer_offset(offset: u64, index: u64);
    set_texture_q<STAGE> => set_texture(texture: Id, index: u64);
    set_sampler_q<STAGE> => set_sampler(sampler: Id, index: u64);
}

drained! {
    commit_d => commit();
    add_handler_d => add_handler(block: Id);
    present_drawable_d => present_drawable(drawable: Id);
    present_drawable_at_time_d => present_drawable_at_time(drawable: Id, at: f64);
    encode_signal_event_d => encode_signal_event(event: Id, value: u64);
    encode_wait_for_event_d => encode_wait_for_event(event: Id, value: u64);
    new_parallel_encoder_d => new_parallel_encoder(descriptor: Id) -> Id;
    new_sub_encoder_d => new_sub_encoder() -> Id;
    end_parallel_encoding_d => end_parallel_encoding();
    parallel_dealloc_d => parallel_dealloc();
}

/// `-endEncoding`: the encoder's calls go to the worker together, so another
/// thread that uses the command buffer next finds them ahead of its own.
unsafe extern "C" fn end_encoding_q(receiver: Id, sel: Sel) {
    unsafe { end_encoding_queued(receiver, sel) };
    send();
}

/// `-setBytes:` and its per-stage spellings: the bytes are copied now, as Metal
/// copies them. A call the original would refuse goes with a null pointer, so
/// the refusal is still traced.
unsafe extern "C" fn set_bytes_q<const STAGE: usize>(
    receiver: Id,
    sel: Sel,
    bytes: *const u8,
    length: u64,
    index: u64,
) {
    let receiver = OwnedObjcRef::retain(receiver);
    let sel = sel as usize;
    let valid = !bytes.is_null() && length > 0 && length <= SET_BYTES_LIMIT;
    // SAFETY: the guest promised `length` readable bytes.
    let copy =
        valid.then(|| unsafe { std::slice::from_raw_parts(bytes, length as usize) }.to_vec());
    enqueue(Box::new(move || {
        let bytes = copy.as_ref().map_or(std::ptr::null(), |copy| copy.as_ptr());
        // SAFETY: the guest's call, with its bytes copied.
        unsafe { set_bytes::<STAGE>(receiver.as_id(), sel as Sel, bytes, length, index) }
    }));
}

/// The objects of a ranged setter, held, or `None` for a null array.
///
/// # Safety
///
/// `objects` is null or points at `range.length` objects.
unsafe fn hold_range(objects: *const Id, range: NsRange) -> Option<Vec<OwnedObjcRef>> {
    if objects.is_null() {
        return None;
    }
    // SAFETY: the caller's promise.
    let objects = unsafe { std::slice::from_raw_parts(objects, range.length as usize) };
    Some(
        objects
            .iter()
            .map(|&object| OwnedObjcRef::retain(object))
            .collect(),
    )
}

fn ids(held: &Option<Vec<OwnedObjcRef>>) -> Option<Vec<Id>> {
    held.as_ref()
        .map(|held| held.iter().map(OwnedObjcRef::as_id).collect())
}

fn pointer_of<T>(values: &Option<Vec<T>>) -> *const T {
    values
        .as_ref()
        .map_or(std::ptr::null(), |values| values.as_ptr())
}

unsafe extern "C" fn set_buffers_q<const STAGE: usize>(
    receiver: Id,
    sel: Sel,
    buffers: *const Id,
    offsets: *const u64,
    range: NsRange,
) {
    let receiver = OwnedObjcRef::retain(receiver);
    let sel = sel as usize;
    // SAFETY: the guest's arrays, `range.length` long.
    let held = unsafe { hold_range(buffers, range) };
    let offsets = (!offsets.is_null())
        .then(|| unsafe { std::slice::from_raw_parts(offsets, range.length as usize) }.to_vec());
    enqueue(Box::new(move || {
        let buffers = ids(&held);
        // SAFETY: the guest's call, with copies of its arrays.
        unsafe {
            set_buffers::<STAGE>(
                receiver.as_id(),
                sel as Sel,
                pointer_of(&buffers),
                pointer_of(&offsets),
                range,
            )
        }
    }));
}

type RangedSetter = unsafe extern "C" fn(Id, Sel, *const Id, NsRange);

/// `-setTextures:withRange:`, `-setSamplerStates:withRange:` and their
/// per-stage spellings, which share a shape.
unsafe fn queue_ranged(
    receiver: Id,
    sel: Sel,
    objects: *const Id,
    range: NsRange,
    setter: RangedSetter,
) {
    let receiver = OwnedObjcRef::retain(receiver);
    let sel = sel as usize;
    // SAFETY: the guest's array, `range.length` long.
    let held = unsafe { hold_range(objects, range) };
    enqueue(Box::new(move || {
        let objects = ids(&held);
        // SAFETY: the guest's call, with a copy of its array.
        unsafe { setter(receiver.as_id(), sel as Sel, pointer_of(&objects), range) }
    }));
}

unsafe extern "C" fn set_textures_q<const STAGE: usize>(
    receiver: Id,
    sel: Sel,
    textures: *const Id,
    range: NsRange,
) {
    unsafe { queue_ranged(receiver, sel, textures, range, set_textures::<STAGE>) }
}

unsafe extern "C" fn set_samplers_q<const STAGE: usize>(
    receiver: Id,
    sel: Sel,
    samplers: *const Id,
    range: NsRange,
) {
    unsafe { queue_ranged(receiver, sel, samplers, range, set_samplers::<STAGE>) }
}

/// A render pass descriptor as it was when the encoder was made: a guest
/// reuses its descriptor, changing it for the next pass right away.
struct PassSnapshot(RenderPassDescriptor);

// SAFETY: the objects it names travel with it as held references, and nothing
// else reads them until the worker begins the pass.
unsafe impl Send for PassSnapshot {}

fn snapshot(descriptor: &RenderPassDescriptor) -> (PassSnapshot, Vec<OwnedObjcRef>) {
    let mut held = Vec::new();
    let attachments = descriptor
        .colors
        .iter()
        .chain([&descriptor.depth, &descriptor.stencil]);
    for attachment in attachments {
        held.push(OwnedObjcRef::retain(attachment.texture));
        held.push(OwnedObjcRef::retain(attachment.resolve_texture));
    }
    held.push(OwnedObjcRef::retain(descriptor.visibility_result_buffer));
    let copy = RenderPassDescriptor {
        colors: descriptor.colors,
        depth: descriptor.depth,
        stencil: descriptor.stencil,
        // The element objects belong to the guest's descriptor.
        color_objects: std::ptr::null_mut(),
        depth_object: std::ptr::null_mut(),
        stencil_object: std::ptr::null_mut(),
        visibility_result_buffer: descriptor.visibility_result_buffer,
        visibility_result_type: descriptor.visibility_result_type,
        render_target_array_length: descriptor.render_target_array_length,
    };
    (PassSnapshot(copy), held)
}

/// `-renderCommandEncoderWithDescriptor:`: the encoder is made now, as before,
/// and its pass begun on the worker from a snapshot of the descriptor.
unsafe extern "C" fn new_render_encoder_q(receiver: Id, _sel: Sel, descriptor: Id) -> Id {
    if descriptor.is_null() {
        return std::ptr::null_mut();
    }
    let encoder = new_encoder(receiver, Kind::Render);
    if encoder.is_null() {
        return encoder;
    }
    // SAFETY: `MTLRenderPassDescriptor` is this crate's class.
    let described = unsafe { object::body::<RenderPassDescriptor>(descriptor) };
    let (pass, held) = snapshot(described);
    let encoder_held = OwnedObjcRef::retain(encoder);
    enqueue(Box::new(move || {
        let (pass, _held) = (pass, held);
        begin_render_pass(encoder_held.as_id(), &pass.0, false);
    }));
    // A guest may make the encoder on one thread and encode on another.
    send();
    encoder
}

/// Swap in the wrappers. Called once, after every class has its methods.
pub(super) fn install() {
    let replace = |class: &'static objc::class::Class, selector: &str, wrapper: *const c_void| {
        let Some(method) = class.own_method(objc::sel::get(selector)) else {
            panic!("{selector} was never registered");
        };
        method.set_implementation(wrapper);
    };
    let encoder = objc::class::lookup(c"RosemaryMTLCommandEncoder").expect("registered");
    let command = objc::class::lookup(c"RosemaryMTLCommandBuffer").expect("registered");
    let parallel =
        objc::class::lookup(c"RosemaryMTLParallelRenderCommandEncoder").expect("registered");

    let queued: &[(&str, *const c_void)] = &[
        ("endEncoding", end_encoding_q as *const c_void),
        (
            "setComputePipelineState:",
            set_compute_pipeline_q as *const c_void,
        ),
        (
            "setRenderPipelineState:",
            set_render_pipeline_q as *const c_void,
        ),
        (
            "setDepthStencilState:",
            set_depth_stencil_state_q as *const c_void,
        ),
        (
            "setStencilReferenceValue:",
            set_stencil_reference_q as *const c_void,
        ),
        (
            "setStencilFrontReferenceValue:backReferenceValue:",
            set_stencil_references_q as *const c_void,
        ),
        ("setViewport:", set_viewport_q as *const c_void),
        ("setScissorRect:", set_scissor_q as *const c_void),
        (
            "setDepthBias:slopeScale:clamp:",
            set_depth_bias_q as *const c_void,
        ),
        (
            "setBlendColorRed:green:blue:alpha:",
            set_blend_color_q as *const c_void,
        ),
        ("setCullMode:", set_raster_value_q as *const c_void),
        (
            "setFrontFacingWinding:",
            set_raster_value_q as *const c_void,
        ),
        ("setTriangleFillMode:", set_raster_value_q as *const c_void),
        ("setDepthClipMode:", set_raster_value_q as *const c_void),
        (
            "setVisibilityResultMode:offset:",
            set_visibility_result_mode_q as *const c_void,
        ),
        ("memoryBarrierWithScope:", memory_barrier_q as *const c_void),
        (
            "memoryBarrierWithScope:afterStages:beforeStages:",
            memory_barrier_q as *const c_void,
        ),
        (
            "memoryBarrierWithResources:count:",
            memory_barrier_q as *const c_void,
        ),
        (
            "memoryBarrierWithResources:count:afterStages:beforeStages:",
            memory_barrier_q as *const c_void,
        ),
        (
            "dispatchThreadgroups:threadsPerThreadgroup:",
            dispatch_threadgroups_q as *const c_void,
        ),
        (
            "dispatchThreads:threadsPerThreadgroup:",
            dispatch_threadgroups_q as *const c_void,
        ),
        (
            "drawPrimitives:vertexStart:vertexCount:instanceCount:",
            draw_primitives_q as *const c_void,
        ),
        (
            "drawPrimitives:vertexStart:vertexCount:instanceCount:baseInstance:",
            draw_primitives_base_q as *const c_void,
        ),
        (
            "drawPrimitives:vertexStart:vertexCount:",
            draw_primitives_once_q as *const c_void,
        ),
        (
            "drawIndexedPrimitives:indexCount:indexType:indexBuffer:indexBufferOffset:instanceCount:",
            draw_indexed_primitives_q as *const c_void,
        ),
        (
            "drawIndexedPrimitives:indexCount:indexType:indexBuffer:indexBufferOffset:instanceCount:baseVertex:baseInstance:",
            draw_indexed_primitives_base_q as *const c_void,
        ),
        (
            "drawIndexedPrimitives:indexCount:indexType:indexBuffer:indexBufferOffset:",
            draw_indexed_primitives_once_q as *const c_void,
        ),
        (
            "drawPrimitives:indirectBuffer:indirectBufferOffset:",
            draw_primitives_indirect_q as *const c_void,
        ),
        (
            "drawIndexedPrimitives:indexType:indexBuffer:indexBufferOffset:indirectBuffer:indirectBufferOffset:",
            draw_indexed_primitives_indirect_q as *const c_void,
        ),
        (
            "copyFromBuffer:sourceOffset:toBuffer:destinationOffset:size:",
            copy_buffer_to_buffer_q as *const c_void,
        ),
        (
            "copyFromBuffer:sourceOffset:sourceBytesPerRow:sourceBytesPerImage:sourceSize:toTexture:destinationSlice:destinationLevel:destinationOrigin:",
            copy_buffer_to_texture_q as *const c_void,
        ),
        (
            "copyFromBuffer:sourceOffset:sourceBytesPerRow:sourceBytesPerImage:sourceSize:toTexture:destinationSlice:destinationLevel:destinationOrigin:options:",
            copy_buffer_to_texture_options_q as *const c_void,
        ),
        (
            "copyFromTexture:sourceSlice:sourceLevel:sourceOrigin:sourceSize:toBuffer:destinationOffset:destinationBytesPerRow:destinationBytesPerImage:",
            copy_texture_to_buffer_q as *const c_void,
        ),
        (
            "copyFromTexture:sourceSlice:sourceLevel:sourceOrigin:sourceSize:toBuffer:destinationOffset:destinationBytesPerRow:destinationBytesPerImage:options:",
            copy_texture_to_buffer_options_q as *const c_void,
        ),
        (
            "copyFromTexture:sourceSlice:sourceLevel:sourceOrigin:sourceSize:toTexture:destinationSlice:destinationLevel:destinationOrigin:",
            copy_texture_to_texture_q as *const c_void,
        ),
        (
            "generateMipmapsForTexture:",
            generate_mipmaps_q as *const c_void,
        ),
    ];
    for &(selector, wrapper) in queued {
        replace(encoder, selector, wrapper);
    }

    macro_rules! per_stage {
        ($($compute:literal, $vertex:literal, $fragment:literal => $wrapper:ident;)*) => {$(
            replace(encoder, $compute, $wrapper::<COMPUTE> as *const c_void);
            replace(encoder, $vertex, $wrapper::<VERTEX> as *const c_void);
            replace(encoder, $fragment, $wrapper::<FRAGMENT> as *const c_void);
        )*};
    }
    per_stage! {
        "setBuffer:offset:atIndex:", "setVertexBuffer:offset:atIndex:",
            "setFragmentBuffer:offset:atIndex:" => set_buffer_q;
        "setBytes:length:atIndex:", "setVertexBytes:length:atIndex:",
            "setFragmentBytes:length:atIndex:" => set_bytes_q;
        "setBufferOffset:atIndex:", "setVertexBufferOffset:atIndex:",
            "setFragmentBufferOffset:atIndex:" => set_buffer_offset_q;
        "setTexture:atIndex:", "setVertexTexture:atIndex:",
            "setFragmentTexture:atIndex:" => set_texture_q;
        "setSamplerState:atIndex:", "setVertexSamplerState:atIndex:",
            "setFragmentSamplerState:atIndex:" => set_sampler_q;
        "setBuffers:offsets:withRange:", "setVertexBuffers:offsets:withRange:",
            "setFragmentBuffers:offsets:withRange:" => set_buffers_q;
        "setTextures:withRange:", "setVertexTextures:withRange:",
            "setFragmentTextures:withRange:" => set_textures_q;
        "setSamplerStates:withRange:", "setVertexSamplerStates:withRange:",
            "setFragmentSamplerStates:withRange:" => set_samplers_q;
    }

    replace(
        command,
        "renderCommandEncoderWithDescriptor:",
        new_render_encoder_q as *const c_void,
    );
    replace(
        command,
        "parallelRenderCommandEncoderWithDescriptor:",
        new_parallel_encoder_d as *const c_void,
    );
    replace(command, "commit", commit_d as *const c_void);
    replace(
        command,
        "addScheduledHandler:",
        add_handler_d as *const c_void,
    );
    replace(
        command,
        "addCompletedHandler:",
        add_handler_d as *const c_void,
    );
    replace(
        command,
        "presentDrawable:",
        present_drawable_d as *const c_void,
    );
    replace(
        command,
        "presentDrawable:atTime:",
        present_drawable_at_time_d as *const c_void,
    );
    replace(
        command,
        "encodeSignalEvent:value:",
        encode_signal_event_d as *const c_void,
    );
    replace(
        command,
        "encodeWaitForEvent:value:",
        encode_wait_for_event_d as *const c_void,
    );

    replace(
        parallel,
        "renderCommandEncoder",
        new_sub_encoder_d as *const c_void,
    );
    replace(
        parallel,
        "endEncoding",
        end_parallel_encoding_d as *const c_void,
    );
    replace(parallel, "dealloc", parallel_dealloc_d as *const c_void);
}
