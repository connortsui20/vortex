use vortex_array::aliases::hash_set::HashSet;
use vortex_array::array::{SparseArray, SparseEncoding};
use vortex_array::encoding::{Encoding, EncodingRef};
use vortex_array::{ArrayData, ArrayLen, IntoArrayData};
use vortex_error::VortexResult;

use crate::compressors::{CompressedArray, CompressionTree, EncodingCompressor};
use crate::downscale::downscale_integer_array;
use crate::{constants, SamplingCompressor};

#[derive(Debug)]
pub struct SparseCompressor;

impl EncodingCompressor for SparseCompressor {
    fn id(&self) -> &str {
        SparseEncoding::ID.as_ref()
    }

    fn cost(&self) -> u8 {
        constants::SPARSE_COST
    }

    fn can_compress(&self, array: &ArrayData) -> Option<&dyn EncodingCompressor> {
        array.is_encoding(SparseEncoding::ID).then_some(self)
    }

    fn compress<'a>(
        &'a self,
        array: &ArrayData,
        like: Option<CompressionTree<'a>>,
        ctx: SamplingCompressor<'a>,
    ) -> VortexResult<CompressedArray<'a>> {
        let sparse_array = SparseArray::try_from(array.clone())?;
        let indices = ctx.auxiliary("indices").compress(
            &downscale_integer_array(sparse_array.indices())?,
            like.as_ref().and_then(|l| l.child(0)),
        )?;
        let values = ctx.named("values").compress(
            &sparse_array.values(),
            like.as_ref().and_then(|l| l.child(1)),
        )?;
        Ok(CompressedArray::compressed(
            SparseArray::try_new(
                indices.array,
                values.array,
                sparse_array.len(),
                sparse_array.fill_scalar(),
            )?
            .into_array(),
            Some(CompressionTree::new(self, vec![indices.path, values.path])),
            array,
        ))
    }

    fn used_encodings(&self) -> HashSet<EncodingRef> {
        HashSet::from([&SparseEncoding as EncodingRef])
    }
}
