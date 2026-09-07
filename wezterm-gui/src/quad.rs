//! The desktop's view of the shared quad machinery, plus the one allocator
//! that knows about GPU render layers.
pub use thinkterm_render::quad::*;
pub use thinkterm_render::vertex::*;

use crate::renderstate::BorrowedLayers;

pub enum TripleLayerQuadAllocator<'a> {
    Gpu(BorrowedLayers),
    Heap(&'a mut HeapQuadAllocator),
    Tee {
        gpu: BorrowedLayers,
        heap: &'a mut HeapQuadAllocator,
    },
}

impl<'a> TripleLayerQuadAllocator<'a> {
    /// The heap's current position, when there is a heap to record into.
    /// `Gpu` allocators cannot be replayed, so a painter drawing straight to
    /// the GPU has nothing to mark.
    pub fn heap_mark(&self) -> Option<HeapQuadMark> {
        match self {
            Self::Gpu(_) => None,
            Self::Heap(heap) => Some(heap.mark()),
            Self::Tee { heap, .. } => Some(heap.mark()),
        }
    }

    /// Map subsequently recorded quad positions while backed by a heap. GPU
    /// vertices are immutable once allocated, so preview callers deliberately
    /// record their complete surface before the final upload.
    pub fn set_heap_position_transform(
        &mut self,
        transform: Option<(QuadClipRect, QuadClipRect)>,
    ) -> bool {
        let Self::Heap(heap) = self else {
            return false;
        };
        let transform = match transform {
            Some((source, target)) => {
                let Some(transform) = QuadPositionTransform::new(source, target) else {
                    return false;
                };
                Some(transform)
            }
            None => None,
        };
        heap.set_position_transform(transform);
        true
    }
}

impl<'a> TripleLayerQuadAllocatorTrait for TripleLayerQuadAllocator<'a> {
    fn allocate(&mut self, layer_num: usize) -> anyhow::Result<QuadImpl<'_>> {
        match self {
            Self::Gpu(b) => b.allocate(layer_num),
            Self::Heap(h) => h.allocate(layer_num),
            Self::Tee { gpu, heap } => {
                let gpu_quad = gpu.allocate(layer_num)?;
                let heap_quad = heap.allocate(layer_num)?;
                match (gpu_quad, heap_quad) {
                    (QuadImpl::Vert(gpu), QuadImpl::Boxed(heap)) => Ok(QuadImpl::Tee(gpu, heap)),
                    _ => unreachable!("tee allocators must pair GPU and heap quads"),
                }
            }
        }
    }

    fn extend_with(&mut self, layer_num: usize, vertices: &[Vertex]) {
        match self {
            Self::Gpu(b) => b.extend_with(layer_num, vertices),
            Self::Heap(h) => h.extend_with(layer_num, vertices),
            Self::Tee { gpu, heap } => {
                gpu.extend_with(layer_num, vertices);
                heap.extend_with(layer_num, vertices);
            }
        }
    }
}
