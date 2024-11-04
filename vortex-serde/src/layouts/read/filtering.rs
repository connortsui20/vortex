use std::any::Any;
use std::fmt::Debug;
use std::sync::Arc;

use arrow_buffer::BooleanBuffer;
use vortex_array::aliases::hash_set::HashSet;
use vortex_array::array::{BoolArray, ConstantArray};
use vortex_array::compute::and;
use vortex_array::stats::ArrayStatistics;
use vortex_array::validity::Validity;
use vortex_array::{Array, IntoArray, IntoArrayVariant};
use vortex_dtype::field::Field;
use vortex_error::{VortexExpect, VortexResult};
use vortex_expr::{split_conjunction, unbox_any, VortexExpr};

use crate::layouts::read::expr_project::expr_project;

#[derive(Debug, Clone)]
pub struct RowFilter {
    conjunction: Vec<Arc<dyn VortexExpr>>,
}

impl RowFilter {
    pub fn new(expr: Arc<dyn VortexExpr>) -> Self {
        let conjunction = split_conjunction(&expr);
        Self { conjunction }
    }

    pub(crate) fn from_conjunction(conjunction: Vec<Arc<dyn VortexExpr>>) -> Self {
        Self { conjunction }
    }

    pub fn only_fields(&self, fields: &[Field]) -> Option<Self> {
        let conj = self
            .conjunction
            .iter()
            .filter_map(|c| expr_project(c, fields))
            .collect::<Vec<_>>();

        if conj.is_empty() {
            None
        } else {
            Some(Self::from_conjunction(conj))
        }
    }
}

impl VortexExpr for RowFilter {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn evaluate(&self, batch: &Array) -> VortexResult<Array> {
        let mut filter_iter = self.conjunction.iter();
        let mut mask = filter_iter
            .next()
            .vortex_expect("must have at least one predicate")
            .evaluate(batch)?;
        for expr in filter_iter {
            if mask.statistics().compute_true_count().unwrap_or_default() == 0 {
                return Ok(ConstantArray::new(false, batch.len()).into_array());
            }

            let new_mask = expr.evaluate(batch)?;
            mask = and(new_mask, mask)?;
        }

        null_as_false(mask.into_bool()?)
    }

    fn collect_references<'a>(&'a self, references: &mut HashSet<&'a Field>) {
        for expr in self.conjunction.iter() {
            expr.collect_references(references);
        }
    }
}

impl PartialEq for RowFilter {
    fn eq(&self, other: &Self) -> bool {
        self.conjunction
            .iter()
            .all(|c| other.conjunction.iter().any(|o| **o == *c.as_any()))
            && other
                .conjunction
                .iter()
                .all(|c| self.conjunction.iter().any(|o| **o == *c.as_any()))
    }
}

impl PartialEq<dyn Any> for RowFilter {
    fn eq(&self, other: &dyn Any) -> bool {
        unbox_any(other)
            .downcast_ref::<Self>()
            .map(|x| x == self)
            .unwrap_or(false)
    }
}

pub fn null_as_false(array: BoolArray) -> VortexResult<Array> {
    Ok(match array.validity() {
        Validity::NonNullable => array.into_array(),
        Validity::AllValid => {
            BoolArray::try_new(array.boolean_buffer(), Validity::NonNullable)?.into_array()
        }
        Validity::AllInvalid => BoolArray::from(BooleanBuffer::new_unset(array.len())).into_array(),
        Validity::Array(v) => {
            let bool_buffer = &array.boolean_buffer() & &v.into_bool()?.boolean_buffer();
            BoolArray::from(bool_buffer).into_array()
        }
    })
}

#[cfg(test)]
mod tests {
    use vortex_array::array::BoolArray;
    use vortex_array::validity::Validity;
    use vortex_array::IntoArrayVariant;

    use super::*;

    #[test]
    fn coerces_nulls() {
        let bool_array = BoolArray::from_vec(
            vec![true, true, false, false],
            Validity::Array(BoolArray::from(vec![true, false, true, false]).into()),
        );
        let non_null_array = null_as_false(bool_array).unwrap().into_bool().unwrap();
        assert_eq!(
            non_null_array.boolean_buffer().iter().collect::<Vec<_>>(),
            vec![true, false, false, false]
        );
    }
}
