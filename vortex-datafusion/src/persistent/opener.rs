use std::sync::{Arc, LazyLock};

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use datafusion::datasource::physical_plan::{FileMeta, FileOpenFuture, FileOpener};
use datafusion_common::Result as DFResult;
use datafusion_physical_expr::{split_conjunction, PhysicalExpr};
use futures::{FutureExt as _, StreamExt, TryStreamExt};
use object_store::ObjectStore;
use vortex_array::Context;
use vortex_expr::datafusion::convert_expr_to_vortex;
use vortex_file::{LayoutContext, LayoutDeserializer, Projection, RowFilter, VortexReadBuilder};
use vortex_io::{IoDispatcher, ObjectStoreReadAt};

/// Share an IO dispatcher across all DataFusion instances.
static IO_DISPATCHER: LazyLock<Arc<IoDispatcher>> =
    LazyLock::new(|| Arc::new(IoDispatcher::default()));

pub struct VortexFileOpener {
    pub ctx: Arc<Context>,
    pub object_store: Arc<dyn ObjectStore>,
    pub projection: Option<Vec<usize>>,
    pub predicate: Option<Arc<dyn PhysicalExpr>>,
    pub arrow_schema: SchemaRef,
}

impl FileOpener for VortexFileOpener {
    fn open(&self, file_meta: FileMeta) -> DFResult<FileOpenFuture> {
        let read_at =
            ObjectStoreReadAt::new(self.object_store.clone(), file_meta.location().clone());

        let mut builder = VortexReadBuilder::new(
            read_at,
            LayoutDeserializer::new(self.ctx.clone(), Arc::new(LayoutContext::default())),
        )
        .with_io_dispatcher(IO_DISPATCHER.clone());

        // We split the predicate and filter out the conjunction members that we can't push down
        let row_filter = self
            .predicate
            .as_ref()
            .map(|filter_expr| {
                split_conjunction(filter_expr)
                    .into_iter()
                    .filter_map(|e| convert_expr_to_vortex(e.clone()).ok())
                    .collect::<Vec<_>>()
            })
            .filter(|conjunction| !conjunction.is_empty())
            .map(RowFilter::from_conjunction);

        if let Some(row_filter) = row_filter {
            builder = builder.with_row_filter(row_filter);
        }

        if let Some(projection) = self.projection.as_ref() {
            builder = builder.with_projection(Projection::new(projection));
        }

        Ok(async {
            Ok(Box::pin(
                builder
                    .build()
                    .await?
                    .map_ok(RecordBatch::try_from)
                    .map(|r| r.and_then(|inner| inner))
                    .map_err(|e| e.into()),
            ) as _)
        }
        .boxed())
    }
}
