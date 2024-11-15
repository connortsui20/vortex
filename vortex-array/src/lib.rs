//! Vortex crate containing core logic for encoding and memory representation of [arrays](ArrayData).
//!
//! At the heart of Vortex are [arrays](ArrayData) and [encodings](crate::encoding::ArrayEncoding).
//! Arrays are typed views of memory buffers that hold [scalars](vortex_scalar::Scalar). These
//! buffers can be held in a number of physical encodings to perform lightweight compression that
//! exploits the particular data distribution of the array's values.
//!
//! Every data type recognized by Vortex also has a canonical physical encoding format, which
//! arrays can be [canonicalized](Canonical) into for ease of access in compute functions.
//!

use std::borrow::Cow;
use std::fmt::{Debug, Display, Formatter};
use std::future::ready;

pub use canonical::*;
pub use context::*;
pub use data::*;
pub use implementation::*;
use itertools::Itertools;
pub use metadata::*;
pub use paste;
use stats::Statistics;
pub use typed::*;
pub use view::*;
use vortex_buffer::Buffer;
use vortex_dtype::DType;
use vortex_error::{vortex_err, vortex_panic, VortexExpect, VortexResult};

use crate::array::visitor::{AcceptArrayVisitor, ArrayVisitor};
use crate::compute::ArrayCompute;
use crate::encoding::{ArrayEncodingRef, EncodingId, EncodingRef};
use crate::iter::{ArrayIterator, ArrayIteratorAdapter};
use crate::stats::{ArrayStatistics, ArrayStatisticsCompute};
use crate::stream::{ArrayStream, ArrayStreamAdapter};
use crate::validity::ArrayValidity;
use crate::variants::ArrayVariants;

pub mod accessor;
pub mod aliases;
pub mod array;
pub mod arrow;
mod canonical;
pub mod compress;
pub mod compute;
mod context;
mod data;
pub mod elementwise;
pub mod encoding;
mod implementation;
pub mod iter;
mod metadata;
pub mod stats;
pub mod stream;
pub mod tree;
mod typed;
pub mod validity;
pub mod variants;
mod view;

pub mod flatbuffers {
    //! Re-exported autogenerated code from the core Vortex flatbuffer definitions.
    pub use vortex_flatbuffers::array::*;
}

/// A central type for all Vortex arrays, which are known length sequences of typed and possibly compressed data.
///
/// This is the main entrypoint for working with in-memory Vortex data, and dispatches work over the underlying encoding or memory representations.
#[derive(Debug, Clone)]
pub struct ArrayData(pub(crate) InnerArrayData);

#[derive(Debug, Clone)]
pub(crate) enum InnerArrayData {
    /// Owned [`ArrayData`] with serialized metadata, backed by heap-allocated memory.
    Owned(OwnedArrayData),
    /// Zero-copy view over flatbuffer-encoded [`ArrayData`] data, created without eager serialization.
    Viewed(ViewedArrayData),
}

impl ArrayData {
    pub fn encoding(&self) -> EncodingRef {
        match &self.0 {
            InnerArrayData::Owned(d) => d.encoding(),
            InnerArrayData::Viewed(v) => v.encoding(),
        }
    }

    /// Returns the number of logical elements in the array.
    #[allow(clippy::same_name_method)]
    pub fn len(&self) -> usize {
        match &self.0 {
            InnerArrayData::Owned(d) => d.len(),
            InnerArrayData::Viewed(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match &self.0 {
            InnerArrayData::Owned(d) => d.is_empty(),
            InnerArrayData::Viewed(v) => v.is_empty(),
        }
    }

    /// Total size of the array in bytes, including all children and buffers.
    pub fn nbytes(&self) -> usize {
        self.with_dyn(|a| a.nbytes())
    }

    pub fn child<'a>(&'a self, idx: usize, dtype: &'a DType, len: usize) -> VortexResult<Self> {
        match &self.0 {
            InnerArrayData::Owned(d) => d.child(idx, dtype, len).cloned(),
            InnerArrayData::Viewed(v) => v
                .child(idx, dtype, len)
                .map(|view| ArrayData(InnerArrayData::Viewed(view))),
        }
    }

    /// Returns a Vec of Arrays with all the array's child arrays.
    pub fn children(&self) -> Vec<ArrayData> {
        match &self.0 {
            InnerArrayData::Owned(d) => d.children().iter().cloned().collect_vec(),
            InnerArrayData::Viewed(v) => v.children(),
        }
    }

    /// Returns the number of child arrays
    pub fn nchildren(&self) -> usize {
        match &self.0 {
            InnerArrayData::Owned(d) => d.nchildren(),
            InnerArrayData::Viewed(v) => v.nchildren(),
        }
    }

    pub fn depth_first_traversal(&self) -> ArrayChildrenIterator {
        ArrayChildrenIterator::new(self.clone())
    }

    /// Count the number of cumulative buffers encoded by self.
    pub fn cumulative_nbuffers(&self) -> usize {
        self.children()
            .iter()
            .map(|child| child.cumulative_nbuffers())
            .sum::<usize>()
            + if self.buffer().is_some() { 1 } else { 0 }
    }

    /// Return the buffer offsets and the total length of all buffers, assuming the given alignment.
    /// This includes all child buffers.
    pub fn all_buffer_offsets(&self, alignment: usize) -> Vec<u64> {
        let mut offsets = vec![];
        let mut offset = 0;

        for col_data in self.depth_first_traversal() {
            if let Some(buffer) = col_data.buffer() {
                offsets.push(offset as u64);

                let buffer_size = buffer.len();
                let aligned_size = (buffer_size + (alignment - 1)) & !(alignment - 1);
                offset += aligned_size;
            }
        }
        offsets.push(offset as u64);

        offsets
    }

    /// Get back the (possibly owned) metadata for the array.
    ///
    /// View arrays will return a reference to their bytes, while heap-backed arrays
    /// must first serialize their metadata, returning an owned byte array to the caller.
    pub fn metadata(&self) -> VortexResult<Cow<[u8]>> {
        match &self.0 {
            InnerArrayData::Owned(array_data) => {
                // Heap-backed arrays must first try and serialize the metadata.
                let owned_meta: Vec<u8> = array_data
                    .metadata()
                    .try_serialize_metadata()?
                    .as_ref()
                    .to_owned();

                Ok(Cow::Owned(owned_meta))
            }
            InnerArrayData::Viewed(array_view) => {
                // View arrays have direct access to metadata bytes.
                array_view
                    .metadata()
                    .ok_or_else(|| vortex_err!("things"))
                    .map(Cow::Borrowed)
            }
        }
    }

    pub fn buffer(&self) -> Option<&Buffer> {
        match &self.0 {
            InnerArrayData::Owned(d) => d.buffer(),
            InnerArrayData::Viewed(v) => v.buffer(),
        }
    }

    pub fn into_buffer(self) -> Option<Buffer> {
        match self.0 {
            InnerArrayData::Owned(d) => d.into_buffer(),
            InnerArrayData::Viewed(v) => v.buffer().cloned(),
        }
    }

    pub fn into_array_iterator(self) -> impl ArrayIterator {
        ArrayIteratorAdapter::new(self.dtype().clone(), std::iter::once(Ok(self)))
    }

    pub fn into_array_stream(self) -> impl ArrayStream {
        ArrayStreamAdapter::new(
            self.dtype().clone(),
            futures_util::stream::once(ready(Ok(self))),
        )
    }

    /// Checks whether array is of a given encoding.
    pub fn is_encoding(&self, id: EncodingId) -> bool {
        self.encoding().id() == id
    }

    #[inline]
    pub fn with_dyn<R, F>(&self, mut f: F) -> R
    where
        F: FnMut(&dyn ArrayTrait) -> R,
    {
        let mut result = None;

        self.encoding()
            .with_dyn(self, &mut |array| {
                // Sanity check that the encoding implements the correct array trait
                debug_assert!(
                    match array.dtype() {
                        DType::Null => array.as_null_array().is_some(),
                        DType::Bool(_) => array.as_bool_array().is_some(),
                        DType::Primitive(..) => array.as_primitive_array().is_some(),
                        DType::Utf8(_) => array.as_utf8_array().is_some(),
                        DType::Binary(_) => array.as_binary_array().is_some(),
                        DType::Struct(..) => array.as_struct_array().is_some(),
                        DType::List(..) => array.as_list_array().is_some(),
                        DType::Extension(..) => array.as_extension_array().is_some(),
                    },
                    "Encoding {} does not implement the variant trait for {}",
                    self.encoding().id(),
                    array.dtype()
                );

                result = Some(f(array));
                Ok(())
            })
            .unwrap_or_else(|err| {
                vortex_panic!(
                    err,
                    "Failed to convert Array to {}",
                    std::any::type_name::<dyn ArrayTrait>()
                )
            });

        // Now we unwrap the optional, which we know to be populated by the closure.
        result.vortex_expect("Failed to get result from Array::with_dyn")
    }
}

/// A depth-first pre-order iterator over a ArrayData.
pub struct ArrayChildrenIterator {
    stack: Vec<ArrayData>,
}

impl ArrayChildrenIterator {
    pub fn new(array: ArrayData) -> Self {
        Self { stack: vec![array] }
    }
}

impl Iterator for ArrayChildrenIterator {
    type Item = ArrayData;

    fn next(&mut self) -> Option<Self::Item> {
        let next = self.stack.pop()?;
        for child in next.children().into_iter().rev() {
            self.stack.push(child);
        }
        Some(next)
    }
}

pub trait ToArrayData {
    fn to_array(&self) -> ArrayData;
}

/// Consume `self` and turn it into an [`ArrayData`] infallibly.
///
/// Implementation of this array should never fail.
pub trait IntoArrayData {
    fn into_array(self) -> ArrayData;
}

pub trait ToOwnedArrayData {
    fn to_owned_array_data(&self) -> OwnedArrayData;
}

/// Collects together the behavior of an array.
pub trait ArrayTrait:
    ArrayEncodingRef
    + ArrayCompute
    + ArrayDType
    + ArrayLen
    + ArrayVariants
    + IntoCanonical
    + ArrayValidity
    + AcceptArrayVisitor
    + ArrayStatistics
    + ArrayStatisticsCompute
    + ToOwnedArrayData
{
    /// Total size of the array in bytes, including all children and buffers.
    fn nbytes(&self) -> usize {
        let mut visitor = NBytesVisitor(0);
        self.accept(&mut visitor)
            .vortex_expect("Failed to get nbytes from Array");
        visitor.0
    }
}

pub trait ArrayDType {
    // TODO(ngates): move into ArrayTrait?
    fn dtype(&self) -> &DType;
}

impl<T: AsRef<ArrayData>> ArrayDType for T {
    fn dtype(&self) -> &DType {
        match &self.as_ref().0 {
            InnerArrayData::Owned(array_data) => array_data.dtype(),
            InnerArrayData::Viewed(array_view) => array_view.dtype(),
        }
    }
}

pub trait ArrayLen {
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool;
}

impl<T: AsRef<ArrayData>> ArrayLen for T {
    fn len(&self) -> usize {
        match &self.as_ref().0 {
            InnerArrayData::Owned(d) => d.len(),
            InnerArrayData::Viewed(v) => v.len(),
        }
    }

    fn is_empty(&self) -> bool {
        match &self.as_ref().0 {
            InnerArrayData::Owned(d) => d.is_empty(),
            InnerArrayData::Viewed(v) => v.is_empty(),
        }
    }
}

struct NBytesVisitor(usize);

impl ArrayVisitor for NBytesVisitor {
    fn visit_child(&mut self, _name: &str, array: &ArrayData) -> VortexResult<()> {
        self.0 += array.with_dyn(|a| a.nbytes());
        Ok(())
    }

    fn visit_buffer(&mut self, buffer: &Buffer) -> VortexResult<()> {
        self.0 += buffer.len();
        Ok(())
    }
}

impl Display for ArrayData {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let prefix = match &self.0 {
            InnerArrayData::Owned(_) => "",
            InnerArrayData::Viewed(_) => "$",
        };
        write!(
            f,
            "{}{}({}, len={})",
            prefix,
            self.encoding().id(),
            self.dtype(),
            self.len()
        )
    }
}

impl ToOwnedArrayData for ArrayData {
    fn to_owned_array_data(&self) -> OwnedArrayData {
        match &self.0 {
            InnerArrayData::Owned(d) => d.clone(),
            InnerArrayData::Viewed(_) => self.with_dyn(|a| a.to_owned_array_data()),
        }
    }
}

impl ToArrayData for OwnedArrayData {
    fn to_array(&self) -> ArrayData {
        ArrayData(InnerArrayData::Owned(self.clone()))
    }
}

impl ToArrayData for ViewedArrayData {
    fn to_array(&self) -> ArrayData {
        ArrayData(InnerArrayData::Viewed(self.clone()))
    }
}

impl IntoArrayData for ViewedArrayData {
    fn into_array(self) -> ArrayData {
        ArrayData(InnerArrayData::Viewed(self))
    }
}

impl From<ArrayData> for OwnedArrayData {
    fn from(value: ArrayData) -> OwnedArrayData {
        match value.0 {
            InnerArrayData::Owned(d) => d,
            InnerArrayData::Viewed(_) => value.with_dyn(|v| v.to_owned_array_data()),
        }
    }
}

impl From<OwnedArrayData> for ArrayData {
    fn from(value: OwnedArrayData) -> ArrayData {
        ArrayData(InnerArrayData::Owned(value))
    }
}

impl<T: AsRef<ArrayData>> ArrayStatistics for T {
    fn statistics(&self) -> &(dyn Statistics + '_) {
        match &self.as_ref().0 {
            InnerArrayData::Owned(d) => d.statistics(),
            InnerArrayData::Viewed(v) => v.statistics(),
        }
    }

    fn inherit_statistics(&self, parent: &dyn Statistics) {
        let stats = self.statistics();
        for (stat, scalar) in parent.to_set() {
            stats.set(stat, scalar);
        }
    }
}
