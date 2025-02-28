use vortex_array::aliases::hash_set::HashSet;
use vortex_array::array::BoolEncoding;
use vortex_array::encoding::{Encoding, EncodingRef};
use vortex_array::{ArrayDType, ArrayData, IntoArrayData, IntoArrayVariant};
use vortex_dtype::DType;
use vortex_dtype::Nullability::NonNullable;
use vortex_error::VortexResult;
use vortex_roaring::{roaring_bool_encode, RoaringBoolEncoding};

use crate::compressors::{CompressedArray, CompressionTree, EncodingCompressor};
use crate::{constants, SamplingCompressor};

#[derive(Debug)]
pub struct RoaringBoolCompressor;

impl EncodingCompressor for RoaringBoolCompressor {
    fn id(&self) -> &str {
        RoaringBoolEncoding::ID.as_ref()
    }

    fn cost(&self) -> u8 {
        constants::ROARING_BOOL_COST
    }

    fn can_compress(&self, array: &ArrayData) -> Option<&dyn EncodingCompressor> {
        // Only support bool arrays
        if array.encoding().id() != BoolEncoding::ID {
            return None;
        }

        // Only support non-nullable bool arrays
        if array.dtype() != &DType::Bool(NonNullable) {
            return None;
        }

        if array.len() > u32::MAX as usize {
            return None;
        }

        Some(self)
    }

    fn compress<'a>(
        &'a self,
        array: &ArrayData,
        _like: Option<CompressionTree<'a>>,
        _ctx: SamplingCompressor<'a>,
    ) -> VortexResult<CompressedArray<'a>> {
        Ok(CompressedArray::compressed(
            roaring_bool_encode(array.clone().into_bool()?)?.into_array(),
            Some(CompressionTree::flat(self)),
            array,
        ))
    }

    fn used_encodings(&self) -> HashSet<EncodingRef> {
        HashSet::from([&RoaringBoolEncoding as EncodingRef])
    }
}
