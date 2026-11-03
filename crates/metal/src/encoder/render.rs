use super::*;

pub(super) unsafe extern "C" fn draw_primitives(
    receiver: Id,
    _sel: Sel,
    primitive: u64,
    vertex_start: u64,
    vertex_count: u64,
    instance_count: u64,
) {
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    let Some((device, command)) = encoder.prepare_draw(primitive, 0) else {
        return;
    };
    // SAFETY: recording into this command buffer.
    encoder.record(|| unsafe {
        device.cmd_draw(
            command,
            vertex_count as u32,
            instance_count.max(1) as u32,
            vertex_start as u32,
            0,
        );
    });
}

pub(super) unsafe extern "C" fn draw_primitives_base(
    receiver: Id,
    _sel: Sel,
    primitive: u64,
    vertex_start: u64,
    vertex_count: u64,
    instance_count: u64,
    base_instance: u64,
) {
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    let Some((device, command)) = encoder.prepare_draw(primitive, base_instance) else {
        return;
    };
    // SAFETY: recording into this command buffer.
    encoder.record(|| unsafe {
        device.cmd_draw(
            command,
            vertex_count as u32,
            instance_count.max(1) as u32,
            vertex_start as u32,
            base_instance as u32,
        );
    });
}

#[allow(clippy::too_many_arguments)]
pub(super) unsafe extern "C" fn draw_indexed_primitives(
    receiver: Id,
    _sel: Sel,
    primitive: u64,
    index_count: u64,
    index_type: u64,
    index_buffer: Id,
    index_offset: u64,
    instance_count: u64,
) {
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    unsafe {
        draw_indexed(
            encoder,
            primitive,
            index_count,
            index_type,
            index_buffer,
            index_offset,
            instance_count,
            0,
            0,
        )
    };
}

#[allow(clippy::too_many_arguments)]
pub(super) unsafe extern "C" fn draw_indexed_primitives_base(
    receiver: Id,
    _sel: Sel,
    primitive: u64,
    index_count: u64,
    index_type: u64,
    index_buffer: Id,
    index_offset: u64,
    instance_count: u64,
    base_vertex: i64,
    base_instance: u64,
) {
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    unsafe {
        draw_indexed(
            encoder,
            primitive,
            index_count,
            index_type,
            index_buffer,
            index_offset,
            instance_count,
            base_vertex,
            base_instance,
        )
    };
}

/// # Safety
///
/// `index_buffer` must be null or an `MTLBuffer` of this crate's.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn draw_indexed(
    encoder: &mut Encoder,
    primitive: u64,
    index_count: u64,
    index_type: u64,
    index_buffer: Id,
    index_offset: u64,
    instance_count: u64,
    base_vertex: i64,
    base_instance: u64,
) {
    if index_buffer.is_null() {
        return;
    }
    let Some((device, command)) = encoder.prepare_draw(primitive, base_instance) else {
        return;
    };
    unsafe {
        bind_index_buffer(
            encoder,
            device,
            command,
            index_type,
            index_buffer,
            index_offset,
        )
    };
    // SAFETY: recording into this command buffer.
    encoder.record(|| unsafe {
        device.cmd_draw_indexed(
            command,
            index_count as u32,
            instance_count.max(1) as u32,
            0,
            base_vertex as i32,
            base_instance as u32,
        );
    });
}

/// `-drawPrimitives:vertexStart:vertexCount:`, one instance.
pub(super) unsafe extern "C" fn draw_primitives_once(
    receiver: Id,
    sel: Sel,
    primitive: u64,
    vertex_start: u64,
    vertex_count: u64,
) {
    unsafe { draw_primitives(receiver, sel, primitive, vertex_start, vertex_count, 1) };
}

/// `-drawIndexedPrimitives:indexCount:indexType:indexBuffer:indexBufferOffset:`,
/// one instance.
pub(super) unsafe extern "C" fn draw_indexed_primitives_once(
    receiver: Id,
    sel: Sel,
    primitive: u64,
    index_count: u64,
    index_type: u64,
    index_buffer: Id,
    index_offset: u64,
) {
    unsafe {
        draw_indexed_primitives(
            receiver,
            sel,
            primitive,
            index_count,
            index_type,
            index_buffer,
            index_offset,
            1,
        )
    };
}

/// `-drawPrimitives:indirectBuffer:indirectBufferOffset:`.
///
/// `MTLDrawPrimitivesIndirectArguments` is `VkDrawIndirectCommand` field for
/// field, so the guest's buffer is read by the device as it is.
pub(super) unsafe extern "C" fn draw_primitives_indirect(
    receiver: Id,
    _sel: Sel,
    primitive: u64,
    indirect: Id,
    offset: u64,
) {
    if indirect.is_null() {
        return;
    }
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    // FIXME (ACCURACY): the base instance is in device memory, so the check a
    // stepped vertex layout needs on it is made against zero.
    let Some((device, command)) = encoder.prepare_draw(primitive, 0) else {
        return;
    };
    // SAFETY: only a buffer of this crate's is ever bound.
    let buffer = unsafe { object::body::<Buffer>(indirect) }.device_buffer();
    encoder.command().retain_resource(indirect);
    // SAFETY: recording into this command buffer.
    encoder.record(|| unsafe { device.cmd_draw_indirect(command, buffer, offset, 1, 16) });
}

/// `-drawIndexedPrimitives:indexType:indexBuffer:indexBufferOffset:indirectBuffer:indirectBufferOffset:`.
///
/// `MTLDrawIndexedPrimitivesIndirectArguments` is likewise
/// `VkDrawIndexedIndirectCommand`.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe extern "C" fn draw_indexed_primitives_indirect(
    receiver: Id,
    _sel: Sel,
    primitive: u64,
    index_type: u64,
    index_buffer: Id,
    index_offset: u64,
    indirect: Id,
    offset: u64,
) {
    if index_buffer.is_null() || indirect.is_null() {
        return;
    }
    // SAFETY: an encoder of this crate's.
    let encoder = unsafe { object::body::<Encoder>(receiver) };
    // FIXME (ACCURACY): as for `-drawPrimitives:indirectBuffer:`.
    let Some((device, command)) = encoder.prepare_draw(primitive, 0) else {
        return;
    };
    unsafe {
        bind_index_buffer(
            encoder,
            device,
            command,
            index_type,
            index_buffer,
            index_offset,
        )
    };
    // SAFETY: only a buffer of this crate's is ever bound.
    let buffer = unsafe { object::body::<Buffer>(indirect) }.device_buffer();
    encoder.command().retain_resource(indirect);
    // SAFETY: recording into this command buffer.
    encoder.record(|| unsafe { device.cmd_draw_indexed_indirect(command, buffer, offset, 1, 20) });
}

/// # Safety
///
/// `index_buffer` must be an `MTLBuffer` of this crate's.
unsafe fn bind_index_buffer(
    encoder: &mut Encoder,
    device: &ash::Device,
    command: vk::CommandBuffer,
    index_type: u64,
    index_buffer: Id,
    index_offset: u64,
) {
    // SAFETY: the caller guarantees the buffer's class.
    let buffer = unsafe { object::body::<Buffer>(index_buffer) };
    encoder.command().retain_resource(index_buffer);
    let binding = (
        buffer.device_buffer(),
        index_offset,
        match index_type {
            1 => vk::IndexType::UINT32,
            _ => vk::IndexType::UINT16,
        },
    );
    if encoder.index_buffer != binding {
        encoder.index_buffer = binding;
        // SAFETY: recording into this command buffer.
        unsafe { device.cmd_bind_index_buffer(command, binding.0, binding.1, binding.2) };
    }
}
