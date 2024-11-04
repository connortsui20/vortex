use std::collections::BTreeSet;
use std::sync::Arc;

use bytes::Bytes;
use itertools::Itertools;
use vortex_dtype::field::Field;
use vortex_dtype::DType;
use vortex_error::{vortex_bail, vortex_err, vortex_panic, VortexResult};
use vortex_expr::{Column, Select};
use vortex_flatbuffers::footer;

use crate::layouts::read::batch::ColumnBatchReader;
use crate::layouts::read::cache::{LazyDeserializedDType, RelativeLayoutCache};
use crate::layouts::read::expr_project::expr_project;
use crate::layouts::read::mask::RowMask;
use crate::layouts::{
    BatchRead, LayoutDeserializer, LayoutId, LayoutReader, LayoutSpec, RowFilter, Scan,
    COLUMN_LAYOUT_ID,
};

#[derive(Debug)]
pub struct ColumnLayoutSpec;

impl LayoutSpec for ColumnLayoutSpec {
    fn id(&self) -> LayoutId {
        COLUMN_LAYOUT_ID
    }

    fn layout(
        &self,
        fb_bytes: Bytes,
        fb_loc: usize,
        scan: Scan,
        layout_serde: LayoutDeserializer,
        message_cache: RelativeLayoutCache,
    ) -> VortexResult<Box<dyn LayoutReader>> {
        Ok(Box::new(ColumnLayout::new(
            fb_bytes,
            fb_loc,
            scan,
            layout_serde,
            message_cache,
        )))
    }
}

/// In memory representation of Columnar NestedLayout.
///
/// Each child represents a column
#[derive(Debug)]
pub struct ColumnLayout {
    fb_bytes: Bytes,
    fb_loc: usize,
    scan: Scan,
    layout_serde: LayoutDeserializer,
    message_cache: RelativeLayoutCache,
    reader: Option<ColumnBatchReader>,
}

impl ColumnLayout {
    pub fn new(
        fb_bytes: Bytes,
        fb_loc: usize,
        scan: Scan,
        layout_serde: LayoutDeserializer,
        message_cache: RelativeLayoutCache,
    ) -> Self {
        Self {
            fb_bytes,
            fb_loc,
            scan,
            layout_serde,
            message_cache,
            reader: None,
        }
    }

    fn flatbuffer(&self) -> footer::Layout {
        unsafe {
            let tab = flatbuffers::Table::new(&self.fb_bytes, self.fb_loc);
            footer::Layout::init_from_table(tab)
        }
    }

    /// Perform minimal amount of work to construct children that can be queried for splits
    fn children_for_splits(&self) -> VortexResult<Vec<Box<dyn LayoutReader>>> {
        let (refs, lazy_dtype) = self.fields_with_dtypes()?;
        let fb_children = self.flatbuffer().children().unwrap_or_default();

        refs.into_iter()
            .map(|field| {
                let resolved_child = lazy_dtype.resolve_field(&field)?;
                let child_loc = fb_children.get(resolved_child)._tab.loc();

                self.layout_serde.read_layout(
                    self.fb_bytes.clone(),
                    child_loc,
                    Scan::new(None),
                    self.message_cache.unknown_dtype(resolved_child as u16),
                )
            })
            .collect::<VortexResult<Vec<_>>>()
    }

    fn column_reader(&self) -> VortexResult<ColumnBatchReader> {
        let (refs, lazy_dtype) = self.fields_with_dtypes()?;
        let fb_children = self.flatbuffer().children().unwrap_or_default();

        let filter_dtype = lazy_dtype.value()?;
        let DType::Struct(s, ..) = filter_dtype else {
            vortex_bail!("Column layout must have struct dtype")
        };

        let mut unhandled_names = Vec::new();
        let mut unhandled_children = Vec::new();
        let mut handled_children = Vec::new();
        let mut handled_names = Vec::new();

        for (field, (name, dtype)) in refs
            .into_iter()
            .zip_eq(s.names().iter().cloned().zip_eq(s.dtypes().iter().cloned()))
        {
            let resolved_child = lazy_dtype.resolve_field(&field)?;
            let child_loc = fb_children.get(resolved_child)._tab.loc();
            let projected_expr = self
                .scan
                .expr
                .as_ref()
                .and_then(|e| expr_project(e, &[field]));

            let handled =
                self.scan.expr.is_none() || (self.scan.expr.is_some() && projected_expr.is_some());

            let child = self.layout_serde.read_layout(
                self.fb_bytes.clone(),
                child_loc,
                Scan::new(projected_expr),
                self.message_cache.relative(
                    resolved_child as u16,
                    Arc::new(LazyDeserializedDType::from_dtype(dtype)),
                ),
            )?;

            if handled {
                handled_children.push(child);
                handled_names.push(name);
            } else {
                unhandled_children.push(child);
                unhandled_names.push(name);
            }
        }

        if !unhandled_names.is_empty() {
            let prf = self
                .scan
                .expr
                .as_ref()
                .and_then(|e| {
                    expr_project(
                        e,
                        &unhandled_names
                            .iter()
                            .map(|n| Field::from(n.as_ref()))
                            .collect::<Vec<_>>(),
                    )
                })
                .ok_or_else(|| {
                    vortex_err!(
                        "Must be able to project {:?} filter into unhandled space {}",
                        self.scan.expr.as_ref(),
                        unhandled_names.iter().format(",")
                    )
                })?;

            handled_children.push(Box::new(ColumnBatchReader::new(
                unhandled_names.into(),
                unhandled_children,
                Some(prf),
                false,
            )));
            handled_names.push("__unhandled".into());
        }

        let top_level_expr = self
            .scan
            .expr
            .as_ref()
            .map(|e| e.as_any().downcast_ref::<RowFilter>().is_some())
            .unwrap_or(false)
            .then(|| {
                Arc::new(RowFilter::from_conjunction(
                    handled_names
                        .iter()
                        .map(|f| Arc::new(Column::new(Field::from(&**f))) as _)
                        .collect(),
                )) as _
            });
        let shortcircuit_siblings = top_level_expr.is_some();
        Ok(ColumnBatchReader::new(
            handled_names.into(),
            handled_children,
            top_level_expr,
            shortcircuit_siblings,
        ))
    }

    /// Get fields referenced by scan expression along with their dtype
    fn fields_with_dtypes(&self) -> VortexResult<(Vec<Field>, Arc<LazyDeserializedDType>)> {
        let fb_children = self.flatbuffer().children().unwrap_or_default();
        let field_refs = self.scan_fields();
        let lazy_dtype = field_refs
            .as_ref()
            .map(|e| self.message_cache.dtype().project(e))
            .unwrap_or_else(|| Ok(self.message_cache.dtype().clone()))?;

        Ok((
            field_refs.unwrap_or_else(|| (0..fb_children.len()).map(Field::from).collect()),
            lazy_dtype,
        ))
    }

    /// Get fields referenced by scan expression preserving order if we're using select to project
    fn scan_fields(&self) -> Option<Vec<Field>> {
        self.scan.expr.as_ref().map(|e| {
            if let Some(se) = e.as_any().downcast_ref::<Select>() {
                match se {
                    Select::Include(i) => i.clone(),
                    Select::Exclude(_) => vortex_panic!("Select::Exclude is not supported"),
                }
            } else {
                e.references().into_iter().cloned().collect::<Vec<_>>()
            }
        })
    }
}

impl LayoutReader for ColumnLayout {
    fn add_splits(&self, row_offset: usize, splits: &mut BTreeSet<usize>) -> VortexResult<()> {
        for child in self.children_for_splits()? {
            child.add_splits(row_offset, splits)?
        }
        Ok(())
    }

    fn read_selection(&mut self, selector: &RowMask) -> VortexResult<Option<BatchRead>> {
        if let Some(r) = &mut self.reader {
            r.read_selection(selector)
        } else {
            self.reader = Some(self.column_reader()?);
            self.read_selection(selector)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::iter;
    use std::sync::{Arc, RwLock};

    use bytes::Bytes;
    use vortex_array::accessor::ArrayAccessor;
    use vortex_array::array::{ChunkedArray, PrimitiveArray, StructArray, VarBinArray};
    use vortex_array::validity::Validity;
    use vortex_array::{ArrayDType, IntoArray, IntoArrayVariant};
    use vortex_dtype::field::Field;
    use vortex_dtype::{DType, Nullability};
    use vortex_expr::{BinaryExpr, Column, Literal, Operator};
    use vortex_schema::projection::Projection;

    use crate::layouts::read::cache::{LazyDeserializedDType, RelativeLayoutCache};
    use crate::layouts::read::layouts::test_read::{filter_read_layout, read_layout};
    use crate::layouts::{
        LayoutDescriptorReader, LayoutDeserializer, LayoutMessageCache, LayoutReader, LayoutWriter,
        RowFilter, Scan,
    };

    async fn layout_and_bytes(
        cache: Arc<RwLock<LayoutMessageCache>>,
        scan: Scan,
    ) -> (Box<dyn LayoutReader>, Box<dyn LayoutReader>, Bytes, usize) {
        let int_array = PrimitiveArray::from((0..100).collect::<Vec<_>>()).into_array();
        let int_dtype = int_array.dtype().clone();
        let chunked = ChunkedArray::try_new(iter::repeat(int_array).take(5).collect(), int_dtype)
            .unwrap()
            .into_array();
        let str_array = VarBinArray::from_vec(
            iter::repeat("test text").take(500).collect(),
            DType::Utf8(Nullability::NonNullable),
        )
        .into_array();
        let len = chunked.len();
        let struct_arr = StructArray::try_new(
            vec!["ints".into(), "strs".into()].into(),
            vec![chunked, str_array],
            len,
            Validity::NonNullable,
        )
        .unwrap()
        .into_array();

        let mut writer = LayoutWriter::new(Vec::new());
        writer = writer.write_array_columns(struct_arr).await.unwrap();
        let written = writer.finalize().await.unwrap();

        let footer = LayoutDescriptorReader::new(LayoutDeserializer::default())
            .read_footer(&written, written.len() as u64)
            .await
            .unwrap();

        let dtype = Arc::new(LazyDeserializedDType::from_bytes(
            footer.dtype_bytes().unwrap(),
            Projection::All,
        ));
        (
            footer
                .layout(scan, RelativeLayoutCache::new(cache.clone(), dtype.clone()))
                .unwrap(),
            footer
                .layout(Scan::new(None), RelativeLayoutCache::new(cache, dtype))
                .unwrap(),
            Bytes::from(written),
            len,
        )
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn read_range() {
        let cache = Arc::new(RwLock::new(LayoutMessageCache::default()));
        let (mut filter_layout, mut project_layout, buf, length) = layout_and_bytes(
            cache.clone(),
            Scan::new(Some(Arc::new(RowFilter::new(Arc::new(BinaryExpr::new(
                Arc::new(Column::new(Field::from("ints"))),
                Operator::Gt,
                Arc::new(Literal::new(10.into())),
            )))))),
        )
        .await;
        let arr = filter_read_layout(
            filter_layout.as_mut(),
            project_layout.as_mut(),
            cache,
            &buf,
            length,
        )
        .pop_front();

        assert!(arr.is_some());
        let prim_arr = arr
            .as_ref()
            .unwrap()
            .with_dyn(|a| a.as_struct_array_unchecked().field(0))
            .unwrap()
            .into_primitive()
            .unwrap();
        let str_arr = arr
            .as_ref()
            .unwrap()
            .with_dyn(|a| a.as_struct_array_unchecked().field(1))
            .unwrap()
            .into_varbinview()
            .unwrap();
        assert_eq!(
            prim_arr.maybe_null_slice::<i32>(),
            &(11..100).collect::<Vec<_>>()
        );
        assert_eq!(
            str_arr
                .with_iterator(|iter| iter
                    .flatten()
                    .map(|s| unsafe { String::from_utf8_unchecked(s.to_vec()) })
                    .collect::<Vec<_>>())
                .unwrap(),
            iter::repeat("test text").take(89).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn read_range_no_filter() {
        let cache = Arc::new(RwLock::new(LayoutMessageCache::default()));
        let (_, mut project_layout, buf, length) =
            layout_and_bytes(cache.clone(), Scan::new(None)).await;
        let arr = read_layout(project_layout.as_mut(), cache, &buf, length).pop_front();

        assert!(arr.is_some());
        let prim_arr = arr
            .as_ref()
            .unwrap()
            .with_dyn(|a| a.as_struct_array_unchecked().field(0))
            .unwrap()
            .into_primitive()
            .unwrap();
        let str_arr = arr
            .as_ref()
            .unwrap()
            .with_dyn(|a| a.as_struct_array_unchecked().field(1))
            .unwrap()
            .into_varbinview()
            .unwrap();
        assert_eq!(
            prim_arr.maybe_null_slice::<i32>(),
            (0..100).collect::<Vec<_>>()
        );
        assert_eq!(
            str_arr
                .with_iterator(|iter| iter
                    .flatten()
                    .map(|s| unsafe { String::from_utf8_unchecked(s.to_vec()) })
                    .collect::<Vec<_>>())
                .unwrap(),
            iter::repeat("test text").take(100).collect::<Vec<_>>()
        );
    }
}
