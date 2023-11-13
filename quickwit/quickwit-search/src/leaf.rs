// Copyright (C) 2023 Quickwit, Inc.
//
// Quickwit is offered under the AGPL v3.0 and as commercial software.
// For commercial licensing, contact us at hello@quickwit.io.
//
// AGPL:
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use std::collections::{HashMap, HashSet};
use std::ops::Bound;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use futures::future::try_join_all;
use itertools::{Either, Itertools};
use quickwit_common::PrettySample;
use quickwit_directories::{CachingDirectory, HotDirectory, StorageDirectory};
use quickwit_doc_mapper::{DocMapper, TermRange, WarmupInfo};
use quickwit_proto::search::{
    CountHits, LeafListTermsResponse, LeafSearchResponse, ListTermsRequest, PartialHit,
    SearchRequest, SortOrder, SortValue, SplitIdAndFooterOffsets, SplitSearchError,
};
use quickwit_query::query_ast::QueryAst;
use quickwit_query::tokenizers::TokenizerManager;
use quickwit_storage::{
    wrap_storage_with_cache, BundleStorage, MemorySizedCache, OwnedBytes, SplitCache, Storage,
};
use tantivy::directory::FileSlice;
use tantivy::fastfield::FastFieldReaders;
use tantivy::schema::{Field, FieldType};
use tantivy::{Index, ReloadPolicy, Searcher, Term};
use tracing::*;

use crate::collector::{make_collector_for_split, make_merge_collector, IncrementalCollector};
use crate::service::SearcherContext;
use crate::SearchError;

#[instrument(skip_all)]
async fn get_split_footer_from_cache_or_fetch(
    index_storage: Arc<dyn Storage>,
    split_and_footer_offsets: &SplitIdAndFooterOffsets,
    footer_cache: &MemorySizedCache<String>,
) -> anyhow::Result<OwnedBytes> {
    {
        let possible_val = footer_cache.get(&split_and_footer_offsets.split_id);
        if let Some(footer_data) = possible_val {
            return Ok(footer_data);
        }
    }
    let split_file = PathBuf::from(format!("{}.split", split_and_footer_offsets.split_id));
    let footer_data_opt = index_storage
        .get_slice(
            &split_file,
            split_and_footer_offsets.split_footer_start as usize
                ..split_and_footer_offsets.split_footer_end as usize,
        )
        .await
        .with_context(|| {
            format!(
                "failed to fetch hotcache and footer from {} for split `{}`",
                index_storage.uri(),
                split_and_footer_offsets.split_id
            )
        })?;

    footer_cache.put(
        split_and_footer_offsets.split_id.to_owned(),
        footer_data_opt.clone(),
    );

    Ok(footer_data_opt)
}

/// Opens a `tantivy::Index` for the given split with several cache layers:
/// - A split footer cache given by `SearcherContext.split_footer_cache`.
/// - A fast fields cache given by `SearcherContext.storage_long_term_cache`.
/// - An ephemeral unbounded cache directory whose lifetime is tied to the returned `Index`.
#[instrument(skip_all, fields(split_footer_start=split_and_footer_offsets.split_footer_start, split_footer_end=split_and_footer_offsets.split_footer_end))]
pub(crate) async fn open_index_with_caches(
    searcher_context: &SearcherContext,
    index_storage: Arc<dyn Storage>,
    split_and_footer_offsets: &SplitIdAndFooterOffsets,
    tokenizer_manager: Option<&TokenizerManager>,
    ephemeral_unbounded_cache: bool,
) -> anyhow::Result<Index> {
    let split_file = PathBuf::from(format!("{}.split", split_and_footer_offsets.split_id));
    let footer_data = get_split_footer_from_cache_or_fetch(
        index_storage.clone(),
        split_and_footer_offsets,
        &searcher_context.split_footer_cache,
    )
    .await?;

    // We wrap the top-level storage with the split cache.
    // This is before the bundle storage: at this point, this storage is reading `.split` files.
    let index_storage_with_split_cache =
        if let Some(split_cache) = searcher_context.split_cache_opt.as_ref() {
            SplitCache::wrap_storage(split_cache.clone(), index_storage.clone())
        } else {
            index_storage.clone()
        };

    let (hotcache_bytes, bundle_storage) = BundleStorage::open_from_split_data(
        index_storage_with_split_cache,
        split_file,
        FileSlice::new(Arc::new(footer_data)),
    )?;
    let bundle_storage_with_cache = wrap_storage_with_cache(
        searcher_context.fast_fields_cache.clone(),
        Arc::new(bundle_storage),
    );
    let directory = StorageDirectory::new(bundle_storage_with_cache);
    let hot_directory = if ephemeral_unbounded_cache {
        let caching_directory = CachingDirectory::new_unbounded(Arc::new(directory));
        HotDirectory::open(caching_directory, hotcache_bytes.read_bytes()?)?
    } else {
        HotDirectory::open(directory, hotcache_bytes.read_bytes()?)?
    };
    let mut index = Index::open(hot_directory)?;
    if let Some(tokenizer_manager) = tokenizer_manager {
        index.set_tokenizers(tokenizer_manager.tantivy_manager().clone());
    }
    index.set_fast_field_tokenizers(
        quickwit_query::get_quickwit_fastfield_normalizer_manager()
            .tantivy_manager()
            .clone(),
    );
    Ok(index)
}

/// Tantivy search does not make it possible to fetch data asynchronously during
/// search.
///
/// It is required to download all required information in advance.
/// This is the role of the `warmup` function.
///
/// The downloaded data depends on the query (which term's posting list is required,
/// are position required too), and the collector.
///
/// * `query` - query is used to extract the terms and their fields which will be loaded from the
/// inverted_index.
///
/// * `term_dict_field_names` - A list of fields, where the whole dictionary needs to be loaded.
/// This is e.g. required for term aggregation, since we don't know in advance which terms are going
/// to be hit.
#[instrument(skip_all)]
pub(crate) async fn warmup(searcher: &Searcher, warmup_info: &WarmupInfo) -> anyhow::Result<()> {
    debug!(warmup_info=?warmup_info);
    let warm_up_terms_future = warm_up_terms(searcher, &warmup_info.terms_grouped_by_field)
        .instrument(debug_span!("warm_up_terms"));
    let warm_up_term_ranges_future =
        warm_up_term_ranges(searcher, &warmup_info.term_ranges_grouped_by_field)
            .instrument(debug_span!("warm_up_term_ranges"));
    let warm_up_term_dict_future =
        warm_up_term_dict_fields(searcher, &warmup_info.term_dict_field_names)
            .instrument(debug_span!("warm_up_term_dicts"));
    let warm_up_fastfields_future = warm_up_fastfields(searcher, &warmup_info.fast_field_names)
        .instrument(debug_span!("warm_up_fastfields"));
    let warm_up_fieldnorms_future = warm_up_fieldnorms(searcher, warmup_info.field_norms)
        .instrument(debug_span!("warm_up_fieldnorms"));
    let warm_up_postings_future = warm_up_postings(searcher, &warmup_info.posting_field_names)
        .instrument(debug_span!("warm_up_postings"));

    tokio::try_join!(
        warm_up_terms_future,
        warm_up_term_ranges_future,
        warm_up_fastfields_future,
        warm_up_term_dict_future,
        warm_up_fieldnorms_future,
        warm_up_postings_future,
    )?;

    Ok(())
}

async fn warm_up_term_dict_fields(
    searcher: &Searcher,
    term_dict_field_names: &HashSet<String>,
) -> anyhow::Result<()> {
    let mut term_dict_fields = Vec::new();
    for term_dict_field_name in term_dict_field_names.iter() {
        let term_dict_field = searcher
            .schema()
            .get_field(term_dict_field_name)
            .with_context(|| {
                format!(
                    "couldn't get field named `{term_dict_field_name}` from schema to warm up \
                     term dicts"
                )
            })?;

        term_dict_fields.push(term_dict_field);
    }

    let mut warm_up_futures = Vec::new();
    for field in term_dict_fields {
        for segment_reader in searcher.segment_readers() {
            let inverted_index = segment_reader.inverted_index(field)?.clone();
            warm_up_futures.push(async move {
                let dict = inverted_index.terms();
                dict.warm_up_dictionary().await
            });
        }
    }
    try_join_all(warm_up_futures).await?;
    Ok(())
}

async fn warm_up_postings(
    searcher: &Searcher,
    field_names: &HashSet<String>,
) -> anyhow::Result<()> {
    let mut fields = Vec::new();
    for field_name in field_names.iter() {
        let field = searcher.schema().get_field(field_name).with_context(|| {
            format!("couldn't get field named `{field_name}` from schema to warm up postings")
        })?;

        fields.push(field);
    }

    let mut warm_up_futures = Vec::new();
    for field in fields {
        for segment_reader in searcher.segment_readers() {
            let inverted_index = segment_reader.inverted_index(field)?.clone();
            warm_up_futures.push(async move { inverted_index.warm_postings_full(false).await });
        }
    }
    try_join_all(warm_up_futures).await?;
    Ok(())
}

async fn warm_up_fastfield(
    fast_field_reader: &FastFieldReaders,
    fast_field_name: &str,
) -> anyhow::Result<()> {
    let columns = fast_field_reader
        .list_dynamic_column_handles(fast_field_name)
        .await?;
    futures::future::try_join_all(
        columns
            .into_iter()
            .map(|col| async move { col.file_slice().read_bytes_async().await }),
    )
    .await?;
    Ok(())
}

/// Populates the short-lived cache with the data for
/// all of the fast fields passed as argument.
async fn warm_up_fastfields(
    searcher: &Searcher,
    fast_field_names: &HashSet<String>,
) -> anyhow::Result<()> {
    let mut warm_up_futures = Vec::new();
    for segment_reader in searcher.segment_readers() {
        let fast_field_reader = segment_reader.fast_fields();
        for fast_field_name in fast_field_names {
            let warm_up_fut = warm_up_fastfield(fast_field_reader, fast_field_name);
            warm_up_futures.push(Box::pin(warm_up_fut));
        }
    }
    futures::future::try_join_all(warm_up_futures).await?;
    Ok(())
}

async fn warm_up_terms(
    searcher: &Searcher,
    terms_grouped_by_field: &HashMap<Field, HashMap<Term, bool>>,
) -> anyhow::Result<()> {
    let mut warm_up_futures = Vec::new();
    for (field, terms) in terms_grouped_by_field {
        for segment_reader in searcher.segment_readers() {
            let inv_idx = segment_reader.inverted_index(*field)?;
            for (term, position_needed) in terms.iter() {
                let inv_idx_clone = inv_idx.clone();
                warm_up_futures
                    .push(async move { inv_idx_clone.warm_postings(term, *position_needed).await });
            }
        }
    }
    try_join_all(warm_up_futures).await?;
    Ok(())
}

async fn warm_up_term_ranges(
    searcher: &Searcher,
    terms_grouped_by_field: &HashMap<Field, HashMap<TermRange, bool>>,
) -> anyhow::Result<()> {
    let mut warm_up_futures = Vec::new();
    for (field, terms) in terms_grouped_by_field {
        for segment_reader in searcher.segment_readers() {
            let inv_idx = segment_reader.inverted_index(*field)?;
            for (term_range, position_needed) in terms.iter() {
                let inv_idx_clone = inv_idx.clone();
                let range = (term_range.start.as_ref(), term_range.end.as_ref());
                warm_up_futures.push(async move {
                    inv_idx_clone
                        .warm_postings_range(range, term_range.limit, *position_needed)
                        .await
                });
            }
        }
    }
    try_join_all(warm_up_futures).await?;
    Ok(())
}

async fn warm_up_fieldnorms(searcher: &Searcher, requires_scoring: bool) -> anyhow::Result<()> {
    if !requires_scoring {
        return Ok(());
    }
    let mut warm_up_futures = Vec::new();
    for field in searcher.schema().fields() {
        for segment_reader in searcher.segment_readers() {
            let fieldnorm_readers = segment_reader.fieldnorms_readers();
            let file_handle_opt = fieldnorm_readers.get_inner_file().open_read(field.0);
            if let Some(file_handle) = file_handle_opt {
                warm_up_futures.push(async move { file_handle.read_bytes_async().await })
            }
        }
    }
    try_join_all(warm_up_futures).await?;
    Ok(())
}

/// Apply a leaf search on a single split.
#[instrument(skip_all, fields(split_id = split.split_id))]
async fn leaf_search_single_split(
    searcher_context: &SearcherContext,
    mut search_request: SearchRequest,
    storage: Arc<dyn Storage>,
    split: SplitIdAndFooterOffsets,
    doc_mapper: Arc<dyn DocMapper>,
) -> crate::Result<LeafSearchResponse> {
    rewrite_request(&mut search_request, &split);
    if let Some(cached_answer) = searcher_context
        .leaf_search_cache
        .get(split.clone(), search_request.clone())
    {
        return Ok(cached_answer);
    }

    let split_id = split.split_id.to_string();
    let index = open_index_with_caches(
        searcher_context,
        storage,
        &split,
        Some(doc_mapper.tokenizer_manager()),
        true,
    )
    .await?;
    let split_schema = index.schema();

    let quickwit_collector = make_collector_for_split(
        split_id.clone(),
        doc_mapper.as_ref(),
        &search_request,
        searcher_context.get_aggregation_limits(),
    )?;
    let query_ast: QueryAst = serde_json::from_str(search_request.query_ast.as_str())
        .map_err(|err| SearchError::InvalidQuery(err.to_string()))?;
    let (query, mut warmup_info) = doc_mapper.query(split_schema, &query_ast, false)?;
    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()?;
    let searcher = reader.searcher();

    let collector_warmup_info = quickwit_collector.warmup_info();
    warmup_info.merge(collector_warmup_info);

    warmup(&searcher, &warmup_info).await?;
    let span = info_span!("tantivy_search");
    let leaf_search_response = crate::run_cpu_intensive(move || {
        let _span_guard = span.enter();
        searcher.search(&query, &quickwit_collector)
    })
    .await
    .map_err(|_| {
        crate::SearchError::Internal(format!("leaf search panicked. split={split_id}"))
    })??;

    searcher_context
        .leaf_search_cache
        .put(split, search_request, leaf_search_response.clone());
    Ok(leaf_search_response)
}

/// Rewrite a request removing parts which incure additional download or computation with no
/// effect.
///
/// This include things such as sorting result by a field or _score when no document is requested,
/// or applying date range when the range covers the entire split.
fn rewrite_request(search_request: &mut SearchRequest, split: &SplitIdAndFooterOffsets) {
    if search_request.max_hits == 0 {
        search_request.sort_fields = vec![];
    }
    rewrite_start_end_time_bounds(
        &mut search_request.start_timestamp,
        &mut search_request.end_timestamp,
        split,
    )
}

pub(crate) fn rewrite_start_end_time_bounds(
    start_timestamp_opt: &mut Option<i64>,
    end_timestamp_opt: &mut Option<i64>,
    split: &SplitIdAndFooterOffsets,
) {
    if let (Some(split_start), Some(split_end)) = (split.timestamp_start, split.timestamp_end) {
        if let Some(start_timestamp) = start_timestamp_opt {
            // both starts are inclusive
            if *start_timestamp <= split_start {
                *start_timestamp_opt = None;
            }
        }
        if let Some(end_timestamp) = end_timestamp_opt {
            // search end is exclusive, split end is inclusive
            if *end_timestamp > split_end {
                *end_timestamp_opt = None;
            }
        }
    }
}

#[derive(Debug, Clone)]
enum CanSplitDoBetter {
    Uninformative,
    SplitIdHigher(Option<String>),
    SplitTimestampHigher(Option<i64>),
    SplitTimestampLower(Option<i64>),
}

impl CanSplitDoBetter {
    /// Create a CanSplitDoBetter from a SearchRequest
    fn from_request(request: &SearchRequest, timestamp_field_name: Option<&str>) -> Self {
        if request.sort_fields.is_empty() {
            CanSplitDoBetter::SplitIdHigher(None)
        } else if let Some((sort_by, timestamp_field)) =
            request.sort_fields.first().zip(timestamp_field_name)
        {
            if sort_by.field_name == timestamp_field {
                if sort_by.sort_order() == SortOrder::Desc {
                    CanSplitDoBetter::SplitTimestampHigher(None)
                } else {
                    CanSplitDoBetter::SplitTimestampLower(None)
                }
            } else {
                CanSplitDoBetter::Uninformative
            }
        } else {
            CanSplitDoBetter::Uninformative
        }
    }

    /// Optimize the order in which splits will get processed based on how it can skip the most
    /// splits.
    ///
    /// The leaf search code contains some logic that makes it possible to skip entire splits
    /// when we are confident they won't make it into top K.
    /// To make this optimization as potent as possible, we sort the splits so that the first splits
    /// are the most likely to fill our Top K.
    /// In the future, as split get more metadata per column, we may be able to do this more than
    /// just for timestamp and "unsorted" request.
    fn optimize_split_order(&self, splits: &mut [SplitIdAndFooterOffsets]) {
        match self {
            CanSplitDoBetter::SplitIdHigher(_) => {
                splits.sort_unstable_by(|a, b| b.split_id.cmp(&a.split_id))
            }
            CanSplitDoBetter::SplitTimestampHigher(_) => {
                splits.sort_unstable_by_key(|split| std::cmp::Reverse(split.timestamp_end()))
            }
            CanSplitDoBetter::SplitTimestampLower(_) => {
                splits.sort_unstable_by_key(|split| split.timestamp_start())
            }
            CanSplitDoBetter::Uninformative => (),
        }
    }

    /// Returns whether the given split can possibly give documents better than the one already
    /// known to match.
    fn can_be_better(&self, split: &SplitIdAndFooterOffsets) -> bool {
        match self {
            CanSplitDoBetter::SplitIdHigher(Some(split_id)) => split.split_id >= *split_id,
            CanSplitDoBetter::SplitTimestampHigher(Some(timestamp)) => {
                split.timestamp_end() > *timestamp
            }
            CanSplitDoBetter::SplitTimestampLower(Some(timestamp)) => {
                split.timestamp_start() < *timestamp
            }
            _ => true,
        }
    }

    /// Record the new worst-of-the-top document, that is, the document which would first be
    /// evicted from the list of best documents, if a better document was found. Only call this
    /// funciton if you have at least max_hits documents already.
    fn record_new_worst_hit(&mut self, hit: &PartialHit) {
        match self {
            CanSplitDoBetter::Uninformative => (),
            CanSplitDoBetter::SplitIdHigher(split_id) => *split_id = Some(hit.split_id.clone()),
            CanSplitDoBetter::SplitTimestampHigher(timestamp) => {
                if let Some(SortValue::I64(timestamp_ns)) = hit.sort_value() {
                    // if we get a timestamp of, says 1.5s, we need to check up to 2s to make
                    // sure we don't throw away something like 1.2s, so we should round up while
                    // dividing.
                    *timestamp = Some(quickwit_common::div_ceil(timestamp_ns, 1_000_000_000));
                }
            }
            CanSplitDoBetter::SplitTimestampLower(timestamp) => {
                if let Some(SortValue::I64(timestamp_ns)) = hit.sort_value() {
                    // if we get a timestamp of, says 1.5s, we need to check down to 1s to make
                    // sure we don't throw away something like 1.7s, so we should truncate,
                    // which is the default behavior of division
                    let timestamp_s = timestamp_ns / 1_000_000_000;
                    *timestamp = Some(timestamp_s);
                }
            }
        }
    }
}

/// `leaf` step of search.
///
/// The leaf search collects all kind of information, and returns a set of
/// [PartialHit](quickwit_proto::search::PartialHit) candidates. The root will be in
/// charge to consolidate, identify the actual final top hits to display, and
/// fetch the actual documents to convert the partial hits into actual Hits.
#[instrument(skip_all, fields(index = ?request.index_id_patterns))]
pub async fn leaf_search(
    searcher_context: Arc<SearcherContext>,
    request: Arc<SearchRequest>,
    index_storage: Arc<dyn Storage>,
    mut splits: Vec<SplitIdAndFooterOffsets>,
    doc_mapper: Arc<dyn DocMapper>,
) -> Result<LeafSearchResponse, SearchError> {
    info!(splits_num = splits.len(), split_offsets = ?PrettySample::new(&splits, 5));

    // In the future this should become `request.aggregation_request.is_some() ||
    // request.exact_count == true`
    let run_all_splits =
        request.aggregation_request.is_some() || request.count_hits() == CountHits::CountAll;

    let split_filter = CanSplitDoBetter::from_request(&request, doc_mapper.timestamp_field_name());
    split_filter.optimize_split_order(&mut splits);

    // Creates a collector which merges responses into one
    let merge_collector =
        make_merge_collector(&request, &searcher_context.get_aggregation_limits())?;
    let incremental_merge_collector = IncrementalCollector::new(merge_collector);

    let split_filter = Arc::new(Mutex::new(split_filter));
    let incremental_merge_collector = Arc::new(Mutex::new(incremental_merge_collector));

    let mut leaf_search_single_split_futures: Vec<_> = Vec::with_capacity(splits.len());

    for split in splits {
        let leaf_split_search_permit = searcher_context.leaf_search_split_semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("Failed to acquire permit. This should never happen! Please, report on https://github.com/quickwit-oss/quickwit/issues.");

        let mut request = (*request).clone();

        if !split_filter.lock().unwrap().can_be_better(&split) {
            if !run_all_splits {
                continue;
            }
            request.max_hits = 0;
            request.start_offset = 0;
            request.sort_fields.clear();
        }

        leaf_search_single_split_futures.push(tokio::spawn(
            leaf_search_single_split_wrapper(
                request,
                searcher_context.clone(),
                index_storage.clone(),
                doc_mapper.clone(),
                split,
                split_filter.clone(),
                incremental_merge_collector.clone(),
                leaf_split_search_permit,
            )
            .in_current_span(),
        ));
    }

    // TODO we could cancel running splits when !run_all_splits and the running split can no longer
    // give better results after some other split answered.
    let split_search_results: Vec<Result<(), _>> =
        futures::future::join_all(leaf_search_single_split_futures).await;

    // we can't use unwrap_or_clone because mutexes aren't Clone
    let mut incremental_merge_collector = match Arc::try_unwrap(incremental_merge_collector) {
        Ok(filter_merger) => filter_merger.into_inner().unwrap(),
        Err(filter_merger) => filter_merger.lock().unwrap().clone(),
    };

    for result in split_search_results {
        // splits that did not panic were already added to the collector
        if let Err(e) = result {
            incremental_merge_collector.add_failed_split(SplitSearchError {
                // we could reasonably add a wrapper to the JoinHandle to give us the
                // split_id anyway
                split_id: "unknown".to_string(),
                error: format!("{}", SearchError::from(e)),
                retryable_error: true,
            })
        }
    }

    crate::run_cpu_intensive(|| incremental_merge_collector.finalize().map_err(Into::into))
        .instrument(info_span!("incremental_merge_finalize"))
        .await
        .context("failed to merge split search responses")?
}

#[allow(clippy::too_many_arguments)]
async fn leaf_search_single_split_wrapper(
    request: SearchRequest,
    searcher_context: Arc<SearcherContext>,
    index_storage: Arc<dyn Storage>,
    doc_mapper: Arc<dyn DocMapper>,
    split: SplitIdAndFooterOffsets,
    split_filter: Arc<Mutex<CanSplitDoBetter>>,
    incremental_merge_collector: Arc<Mutex<IncrementalCollector>>,
    leaf_split_search_permit: tokio::sync::OwnedSemaphorePermit,
) {
    crate::SEARCH_METRICS.leaf_searches_splits_total.inc();
    let timer = crate::SEARCH_METRICS
        .leaf_search_split_duration_secs
        .start_timer();
    let leaf_search_single_split_res = leaf_search_single_split(
        &searcher_context,
        request,
        index_storage,
        split.clone(),
        doc_mapper,
    )
    .await;

    // We explicitly drop it, to highlight it to the reader
    std::mem::drop(leaf_split_search_permit);

    if leaf_search_single_split_res.is_ok() {
        timer.observe_duration();
    }

    let mut locked_incremental_merge_collector = incremental_merge_collector.lock().unwrap();
    match leaf_search_single_split_res {
        Ok(split_search_res) => locked_incremental_merge_collector.add_split(split_search_res),
        Err(err) => locked_incremental_merge_collector.add_failed_split(SplitSearchError {
            split_id: split.split_id.clone(),
            error: format!("{err}"),
            retryable_error: true,
        }),
    }
    if let Some(last_hit) = locked_incremental_merge_collector.peek_worst_hit() {
        split_filter.lock().unwrap().record_new_worst_hit(last_hit);
    }
}

/// Apply a leaf list terms on a single split.
#[instrument(skip_all, fields(split_id = split.split_id))]
async fn leaf_list_terms_single_split(
    searcher_context: &SearcherContext,
    search_request: &ListTermsRequest,
    storage: Arc<dyn Storage>,
    split: SplitIdAndFooterOffsets,
) -> crate::Result<LeafListTermsResponse> {
    let index = open_index_with_caches(searcher_context, storage, &split, None, true).await?;
    let split_schema = index.schema();
    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()?;
    let searcher = reader.searcher();

    let field = split_schema
        .get_field(&search_request.field)
        .with_context(|| {
            format!(
                "couldn't get field named {:?} from schema to list terms",
                search_request.field
            )
        })?;

    let field_type = split_schema.get_field_entry(field).field_type();
    let start_term: Option<Term> = search_request
        .start_key
        .as_ref()
        .map(|data| term_from_data(field, field_type, data));
    let end_term: Option<Term> = search_request
        .end_key
        .as_ref()
        .map(|data| term_from_data(field, field_type, data));

    let mut segment_results = Vec::new();
    for segment_reader in searcher.segment_readers() {
        let inverted_index = segment_reader.inverted_index(field)?.clone();
        let dict = inverted_index.terms();
        dict.file_slice_for_range(
            (
                start_term
                    .as_ref()
                    .map(Term::serialized_value_bytes)
                    .map(Bound::Included)
                    .unwrap_or(Bound::Unbounded),
                end_term
                    .as_ref()
                    .map(Term::serialized_value_bytes)
                    .map(Bound::Excluded)
                    .unwrap_or(Bound::Unbounded),
            ),
            search_request.max_hits,
        )
        .read_bytes_async()
        .await
        .with_context(|| "failed to load sstable range")?;

        let mut range = dict.range();
        if let Some(limit) = search_request.max_hits {
            range = range.limit(limit);
        }
        if let Some(start_term) = &start_term {
            range = range.ge(start_term.serialized_value_bytes())
        }
        if let Some(end_term) = &end_term {
            range = range.lt(end_term.serialized_value_bytes())
        }
        let mut stream = range
            .into_stream()
            .with_context(|| "failed to create stream over sstable")?;
        let mut segment_result: Vec<Vec<u8>> =
            Vec::with_capacity(search_request.max_hits.unwrap_or(0) as usize);
        while stream.advance() {
            segment_result.push(term_to_data(field, field_type, stream.key()));
        }
        segment_results.push(segment_result);
    }

    let merged_iter = segment_results.into_iter().kmerge().dedup();
    let merged_results: Vec<Vec<u8>> = if let Some(limit) = search_request.max_hits {
        merged_iter.take(limit as usize).collect()
    } else {
        merged_iter.collect()
    };

    Ok(LeafListTermsResponse {
        num_hits: merged_results.len() as u64,
        terms: merged_results,
        num_attempted_splits: 1,
        failed_splits: Vec::new(),
    })
}

fn term_from_data(field: Field, field_type: &FieldType, data: &[u8]) -> Term {
    let mut term = Term::from_field_bool(field, false);
    term.clear_with_type(field_type.value_type());
    term.append_bytes(data);
    term
}

fn term_to_data(field: Field, field_type: &FieldType, field_value: &[u8]) -> Vec<u8> {
    let mut term = Term::from_field_bool(field, false);
    term.clear_with_type(field_type.value_type());
    term.append_bytes(field_value);
    term.serialized_term().to_vec()
}

/// `leaf` step of list terms.
#[instrument(skip_all, fields(index = ?request.index_id))]
pub async fn leaf_list_terms(
    searcher_context: Arc<SearcherContext>,
    request: &ListTermsRequest,
    index_storage: Arc<dyn Storage>,
    splits: &[SplitIdAndFooterOffsets],
) -> Result<LeafListTermsResponse, SearchError> {
    info!(split_offsets = ?PrettySample::new(splits, 5));
    let leaf_search_single_split_futures: Vec<_> = splits
        .iter()
        .map(|split| {
            let index_storage_clone = index_storage.clone();
            let searcher_context_clone = searcher_context.clone();
            async move {
                let _leaf_split_search_permit = searcher_context_clone.leaf_search_split_semaphore.clone()
                    .acquire_owned()
                    .await
                    .expect("Failed to acquire permit. This should never happen! Please, report on https://github.com/quickwit-oss/quickwit/issues.");
                // TODO dedicated counter and timer?
                crate::SEARCH_METRICS.leaf_searches_splits_total.inc();
                let timer = crate::SEARCH_METRICS
                    .leaf_search_split_duration_secs
                    .start_timer();
                let leaf_search_single_split_res = leaf_list_terms_single_split(
                    &searcher_context_clone,
                    request,
                    index_storage_clone,
                    split.clone(),
                )
                .await;
                timer.observe_duration();
                leaf_search_single_split_res.map_err(|err| (split.split_id.clone(), err))
            }
        })
        .collect();

    let split_search_results = futures::future::join_all(leaf_search_single_split_futures).await;

    let (split_search_responses, errors): (Vec<LeafListTermsResponse>, Vec<(String, SearchError)>) =
        split_search_results
            .into_iter()
            .partition_map(|split_search_res| match split_search_res {
                Ok(split_search_resp) => Either::Left(split_search_resp),
                Err(err) => Either::Right(err),
            });

    let merged_iter = split_search_responses
        .into_iter()
        .map(|leaf_search_response| leaf_search_response.terms)
        .kmerge()
        .dedup();
    let terms: Vec<Vec<u8>> = if let Some(limit) = request.max_hits {
        merged_iter.take(limit as usize).collect()
    } else {
        merged_iter.collect()
    };

    let failed_splits = errors
        .into_iter()
        .map(|(split_id, err)| SplitSearchError {
            split_id,
            error: err.to_string(),
            retryable_error: true,
        })
        .collect();
    let merged_search_response = LeafListTermsResponse {
        num_hits: terms.len() as u64,
        terms,
        num_attempted_splits: splits.len() as u64,
        failed_splits,
    };

    Ok(merged_search_response)
}
