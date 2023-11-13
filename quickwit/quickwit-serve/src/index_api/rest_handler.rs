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

use std::sync::Arc;

use bytes::Bytes;
use hyper::header::CONTENT_TYPE;
use quickwit_common::uri::Uri;
use quickwit_config::{
    load_source_config_from_user_config, ConfigFormat, NodeConfig, SourceConfig, SourceParams,
    CLI_INGEST_SOURCE_ID, INGEST_API_SOURCE_ID,
};
use quickwit_doc_mapper::{analyze_text, TokenizerConfig};
use quickwit_index_management::{IndexService, IndexServiceError};
use quickwit_metastore::{
    IndexMetadata, IndexMetadataResponseExt, ListIndexesMetadataResponseExt, ListSplitsQuery,
    ListSplitsRequestExt, ListSplitsResponseExt, Split, SplitInfo, SplitState,
};
use quickwit_proto::metastore::{
    DeleteSourceRequest, EntityKind, IndexMetadataRequest, ListIndexesMetadataRequest,
    ListSplitsRequest, MarkSplitsForDeletionRequest, MetastoreError, MetastoreResult,
    MetastoreService, MetastoreServiceClient, ResetSourceCheckpointRequest, ToggleSourceRequest,
};
use quickwit_proto::types::IndexUid;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::info;
use warp::{Filter, Rejection};

use crate::format::extract_format_from_qs;
use crate::json_api_response::make_json_api_response;
use crate::simple_list::{from_simple_list, to_simple_list};
use crate::with_arg;

#[derive(utoipa::OpenApi)]
#[openapi(
    paths(
        create_index,
        clear_index,
        delete_index,
        get_indexes_metadatas,
        list_splits,
        describe_index,
        mark_splits_for_deletion,
        create_source,
        reset_source_checkpoint,
        toggle_source,
        delete_source,
    ),
    components(schemas(ToggleSource, SplitsForDeletion, IndexStats))
)]
pub struct IndexApi;

pub fn index_management_handlers(
    index_service: IndexService,
    node_config: Arc<NodeConfig>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = Rejection> + Clone {
    // Indexes handlers.
    get_index_metadata_handler(index_service.metastore())
        .or(get_indexes_metadatas_handler(index_service.metastore()))
        .or(create_index_handler(index_service.clone(), node_config))
        .or(clear_index_handler(index_service.clone()))
        .or(delete_index_handler(index_service.clone()))
        // Splits handlers
        .or(list_splits_handler(index_service.metastore()))
        .or(describe_index_handler(index_service.metastore()))
        .or(mark_splits_for_deletion_handler(index_service.metastore()))
        // Sources handlers.
        .or(reset_source_checkpoint_handler(index_service.metastore()))
        .or(toggle_source_handler(index_service.metastore()))
        .or(create_source_handler(index_service.clone()))
        .or(get_source_handler(index_service.metastore()))
        .or(delete_source_handler(index_service.metastore()))
        // Tokenizer handlers.
        .or(analyze_request_handler())
}

fn json_body<T: DeserializeOwned + Send>(
) -> impl Filter<Extract = (T,), Error = warp::Rejection> + Clone {
    warp::body::content_length_limit(1024 * 1024).and(warp::body::json())
}

#[derive(Debug, Error)]
#[error(
    "unsupported content-type header. choices are application/json, application/toml and \
     application/yaml"
)]
pub struct UnsupportedContentType;
impl warp::reject::Reject for UnsupportedContentType {}

pub fn config_format_filter() -> impl Filter<Extract = (ConfigFormat,), Error = Rejection> + Copy {
    warp::filters::header::optional::<mime_guess::Mime>(CONTENT_TYPE.as_str()).and_then(
        |mime_opt: Option<mime_guess::Mime>| {
            if let Some(mime) = mime_opt {
                let config_format = match mime.subtype().as_str() {
                    "json" => ConfigFormat::Json,
                    "yaml" => ConfigFormat::Yaml,
                    "toml" => ConfigFormat::Toml,
                    _ => {
                        return futures::future::err(warp::reject::custom(UnsupportedContentType));
                    }
                };
                return futures::future::ok(config_format);
            }
            futures::future::ok(ConfigFormat::Json)
        },
    )
}

fn get_index_metadata_handler(
    metastore: MetastoreServiceClient,
) -> impl Filter<Extract = (impl warp::Reply,), Error = Rejection> + Clone {
    warp::path!("indexes" / String)
        .and(warp::get())
        .and(with_arg(metastore))
        .then(get_index_metadata)
        .and(extract_format_from_qs())
        .map(make_json_api_response)
}

async fn get_index_metadata(
    index_id: String,
    mut metastore: MetastoreServiceClient,
) -> MetastoreResult<IndexMetadata> {
    info!(index_id = %index_id, "get-index-metadata");
    let index_metadata_request = IndexMetadataRequest::for_index_id(index_id.to_string());
    let index_metadata = metastore
        .index_metadata(index_metadata_request)
        .await?
        .deserialize_index_metadata()?;
    Ok(index_metadata)
}

fn get_indexes_metadatas_handler(
    metastore: MetastoreServiceClient,
) -> impl Filter<Extract = (impl warp::Reply,), Error = Rejection> + Clone {
    warp::path!("indexes")
        .and(warp::get())
        .and(with_arg(metastore))
        .then(get_indexes_metadatas)
        .and(extract_format_from_qs())
        .map(make_json_api_response)
}

/// Describes an index with its main information and statistics.
#[derive(Serialize, Deserialize, utoipa::ToSchema)]
struct IndexStats {
    pub index_id: String,
    #[schema(value_type = String)]
    pub index_uri: Uri,
    pub num_published_splits: usize,
    pub size_published_splits: u64,
    pub num_published_docs: u64,
    pub size_published_docs_uncompressed: u64,
    pub timestamp_field_name: Option<String>,
    pub min_timestamp: Option<i64>,
    pub max_timestamp: Option<i64>,
}

#[utoipa::path(
    get,
    tag = "Indexes",
    path = "/indexes/{index_id}/describe",
    responses(
        (status = 200, description = "Successfully fetched stats about Index.", body = IndexStats)
    ),
    params(
        ("index_id" = String, Path, description = "The index ID to describe."),
    )
)]

/// Describes an index.
async fn describe_index(
    index_id: String,
    mut metastore: MetastoreServiceClient,
) -> MetastoreResult<IndexStats> {
    let index_metadata_request = IndexMetadataRequest::for_index_id(index_id.to_string());
    let index_metadata = metastore
        .index_metadata(index_metadata_request)
        .await?
        .deserialize_index_metadata()?;
    let query = ListSplitsQuery::for_index(index_metadata.index_uid.clone());
    let list_splits_request = ListSplitsRequest::try_from_list_splits_query(query)?;
    let splits = metastore
        .list_splits(list_splits_request)
        .await?
        .deserialize_splits()?;
    let published_splits: Vec<Split> = splits
        .into_iter()
        .filter(|split| split.split_state == SplitState::Published)
        .collect();
    let mut total_num_docs = 0;
    let mut total_num_bytes = 0;
    let mut total_uncompressed_num_bytes = 0;
    let mut min_timestamp: Option<i64> = None;
    let mut max_timestamp: Option<i64> = None;

    for split in &published_splits {
        total_num_docs += split.split_metadata.num_docs as u64;
        total_num_bytes += split.split_metadata.footer_offsets.end;
        total_uncompressed_num_bytes += split.split_metadata.uncompressed_docs_size_in_bytes;

        if let Some(time_range) = &split.split_metadata.time_range {
            min_timestamp = min_timestamp
                .min(Some(*time_range.start()))
                .or(Some(*time_range.start()));
            max_timestamp = max_timestamp
                .max(Some(*time_range.end()))
                .or(Some(*time_range.end()));
        }
    }

    let index_config = index_metadata.into_index_config();
    let index_stats = IndexStats {
        index_id,
        index_uri: index_config.index_uri.clone(),
        num_published_splits: published_splits.len(),
        size_published_splits: total_num_bytes,
        num_published_docs: total_num_docs,
        size_published_docs_uncompressed: total_uncompressed_num_bytes,
        timestamp_field_name: index_config.doc_mapping.timestamp_field,
        min_timestamp,
        max_timestamp,
    };

    Ok(index_stats)
}

fn describe_index_handler(
    metastore: MetastoreServiceClient,
) -> impl Filter<Extract = (impl warp::Reply,), Error = Rejection> + Clone {
    warp::path!("indexes" / String / "describe")
        .and(warp::get())
        .and(with_arg(metastore))
        .then(describe_index)
        .and(extract_format_from_qs())
        .map(make_json_api_response)
}

/// This struct represents the QueryString passed to
/// the rest API to filter splits.
#[derive(Debug, Clone, Deserialize, Serialize, utoipa::IntoParams, utoipa::ToSchema, Default)]
#[into_params(parameter_in = Query)]
pub struct ListSplitsQueryParams {
    /// If set, define the number of splits to skip
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub offset: Option<usize>,
    /// If set, restrict maximum number of splits to retrieve
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub limit: Option<usize>,
    /// A specific split state(s) to filter by.
    #[serde(deserialize_with = "from_simple_list")]
    #[serde(serialize_with = "to_simple_list")]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub split_states: Option<Vec<SplitState>>,
    /// If set, restrict splits to documents with a `timestamp >= start_timestamp`.
    /// This timestamp is in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub start_timestamp: Option<i64>,
    /// If set, restrict splits to documents with a `timestamp < end_timestamp`.
    /// This timestamp is in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub end_timestamp: Option<i64>,
    /// If set, restrict splits whose creation dates are before this date.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub end_create_timestamp: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, utoipa::ToSchema)]
pub struct ListSplitsResponse {
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub size: usize,
    #[serde(default)]
    pub splits: Vec<Split>,
}

#[utoipa::path(
    get,
    tag = "Indexes",
    path = "/indexes/{index_id}/splits",
    responses(
        (status = 200, description = "Successfully fetched splits.", body = ListSplitsResponse)
    ),
    params(
        ListSplitsQueryParams,
        ("index_id" = String, Path, description = "The index ID to retrieve delete tasks for."),
    )
)]

/// Get splits.
async fn list_splits(
    index_id: String,
    list_split_query: ListSplitsQueryParams,
    mut metastore: MetastoreServiceClient,
) -> MetastoreResult<ListSplitsResponse> {
    let index_metadata_request = IndexMetadataRequest::for_index_id(index_id.to_string());
    let index_uid: IndexUid = metastore
        .index_metadata(index_metadata_request)
        .await?
        .deserialize_index_metadata()?
        .index_uid;
    info!(index_id = %index_id, list_split_query = ?list_split_query, "get-splits");
    let mut query = ListSplitsQuery::for_index(index_uid);
    let mut offset = 0;
    if let Some(offset_value) = list_split_query.offset {
        query = query.with_offset(offset_value);
        offset = offset_value;
    }
    if let Some(limit) = list_split_query.limit {
        query = query.with_limit(limit);
    }
    if let Some(split_states) = list_split_query.split_states {
        query = query.with_split_states(split_states);
    }
    if let Some(start_timestamp) = list_split_query.start_timestamp {
        query = query.with_time_range_start_gte(start_timestamp);
    }
    if let Some(end_timestamp) = list_split_query.end_timestamp {
        query = query.with_time_range_end_lt(end_timestamp);
    }
    if let Some(end_created_timestamp) = list_split_query.end_create_timestamp {
        query = query.with_create_timestamp_lt(end_created_timestamp);
    }
    let list_splits_request = ListSplitsRequest::try_from_list_splits_query(query)?;
    let splits = metastore
        .list_splits(list_splits_request)
        .await?
        .deserialize_splits()?;
    Ok(ListSplitsResponse {
        offset,
        size: splits.len(),
        splits,
    })
}

fn list_splits_handler(
    metastore: MetastoreServiceClient,
) -> impl Filter<Extract = (impl warp::Reply,), Error = Rejection> + Clone {
    warp::path!("indexes" / String / "splits")
        .and(warp::get())
        .and(serde_qs::warp::query(serde_qs::Config::default()))
        .and(with_arg(metastore))
        .then(list_splits)
        .and(extract_format_from_qs())
        .map(make_json_api_response)
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct SplitsForDeletion {
    pub split_ids: Vec<String>,
}

#[utoipa::path(
    put,
    tag = "Splits",
    path = "/indexes/{index_id}/splits/mark-for-deletion",
    request_body = SplitsForDeletion,
    responses(
        (status = 200, description = "Successfully marked splits for deletion.")
    ),
    params(
        ("index_id" = String, Path, description = "The index ID to mark splits for deletion for."),
    )
)]
/// Marks splits for deletion.
async fn mark_splits_for_deletion(
    index_id: String,
    splits_for_deletion: SplitsForDeletion,
    mut metastore: MetastoreServiceClient,
) -> MetastoreResult<()> {
    let index_metadata_request = IndexMetadataRequest::for_index_id(index_id.to_string());
    let index_uid: IndexUid = metastore
        .index_metadata(index_metadata_request)
        .await?
        .deserialize_index_metadata()?
        .index_uid;
    info!(index_id = %index_id, splits_ids = ?splits_for_deletion.split_ids, "mark-splits-for-deletion");
    let split_ids: Vec<String> = splits_for_deletion
        .split_ids
        .iter()
        .map(|split_id| split_id.to_string())
        .collect();
    let mark_splits_for_deletion_request =
        MarkSplitsForDeletionRequest::new(index_uid, split_ids.clone());
    metastore
        .mark_splits_for_deletion(mark_splits_for_deletion_request)
        .await?;
    Ok(())
}

fn mark_splits_for_deletion_handler(
    metastore: MetastoreServiceClient,
) -> impl Filter<Extract = (impl warp::Reply,), Error = Rejection> + Clone {
    warp::path!("indexes" / String / "splits" / "mark-for-deletion")
        .and(warp::put())
        .and(json_body())
        .and(with_arg(metastore))
        .then(mark_splits_for_deletion)
        .and(extract_format_from_qs())
        .map(make_json_api_response)
}

#[utoipa::path(
    get,
    tag = "Indexes",
    path = "/indexes",
    responses(
        // We return `VersionedIndexMetadata` as it's the serialized model view.
        (status = 200, description = "Successfully fetched all indexes.", body = [VersionedIndexMetadata])
    ),
)]
/// Gets indexes metadata.
async fn get_indexes_metadatas(
    mut metastore: MetastoreServiceClient,
) -> MetastoreResult<Vec<IndexMetadata>> {
    metastore
        .list_indexes_metadata(ListIndexesMetadataRequest::all())
        .await
        .and_then(|response| response.deserialize_indexes_metadata())
}

#[derive(Deserialize, utoipa::IntoParams, utoipa::ToSchema)]
#[into_params(parameter_in = Query)]
struct CreateIndexQueryParams {
    #[serde(default)]
    overwrite: bool,
}

fn create_index_handler(
    index_service: IndexService,
    node_config: Arc<NodeConfig>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = Rejection> + Clone {
    warp::path!("indexes")
        .and(warp::post())
        .and(serde_qs::warp::query(serde_qs::Config::default()))
        .and(config_format_filter())
        .and(warp::body::content_length_limit(1024 * 1024))
        .and(warp::filters::body::bytes())
        .and(with_arg(index_service))
        .and(with_arg(node_config))
        .then(create_index)
        .and(extract_format_from_qs())
        .map(make_json_api_response)
}

#[utoipa::path(
    post,
    tag = "Indexes",
    path = "/indexes",
    request_body = VersionedIndexConfig,
    responses(
        // We return `VersionedIndexMetadata` as it's the serialized model view.
        (status = 200, description = "Successfully created index.", body = VersionedIndexMetadata)
    ),
    params(
        CreateIndexQueryParams,
    )
)]
/// Creates index.
async fn create_index(
    create_index_query_params: CreateIndexQueryParams,
    config_format: ConfigFormat,
    index_config_bytes: Bytes,
    mut index_service: IndexService,
    node_config: Arc<NodeConfig>,
) -> Result<IndexMetadata, IndexServiceError> {
    let index_config = quickwit_config::load_index_config_from_user_config(
        config_format,
        &index_config_bytes,
        &node_config.default_index_root_uri,
    )
    .map_err(IndexServiceError::InvalidConfig)?;
    info!(index_id = %index_config.index_id, overwrite = create_index_query_params.overwrite, "create-index");
    index_service
        .create_index(index_config, create_index_query_params.overwrite)
        .await
}

fn clear_index_handler(
    index_service: IndexService,
) -> impl Filter<Extract = (impl warp::Reply,), Error = Rejection> + Clone {
    warp::path!("indexes" / String / "clear")
        .and(warp::put())
        .and(with_arg(index_service))
        .then(clear_index)
        .and(extract_format_from_qs())
        .map(make_json_api_response)
}

#[utoipa::path(
    put,
    tag = "Indexes",
    path = "/indexes/{index_id}/clear",
    responses(
        (status = 200, description = "Successfully cleared index.")
    ),
    params(
        ("index_id" = String, Path, description = "The index ID to clear."),
    )
)]
/// Removes all of the data (splits, queued document) associated with the index, but keeps the index
/// configuration. (See also, `delete-index`).
async fn clear_index(
    index_id: String,
    mut index_service: IndexService,
) -> Result<(), IndexServiceError> {
    info!(index_id = %index_id, "clear-index");
    index_service.clear_index(&index_id).await
}

#[derive(Deserialize, utoipa::IntoParams, utoipa::ToSchema)]
#[into_params(parameter_in = Query)]
struct DeleteIndexQueryParam {
    #[serde(default)]
    dry_run: bool,
}

fn delete_index_handler(
    index_service: IndexService,
) -> impl Filter<Extract = (impl warp::Reply,), Error = Rejection> + Clone {
    warp::path!("indexes" / String)
        .and(warp::delete())
        .and(serde_qs::warp::query(serde_qs::Config::default()))
        .and(with_arg(index_service))
        .then(delete_index)
        .and(extract_format_from_qs())
        .map(make_json_api_response)
}

#[utoipa::path(
    delete,
    tag = "Indexes",
    path = "/indexes/{index_id}",
    responses(
        // We return `VersionedIndexMetadata` as it's the serialized model view.
        (status = 200, description = "Successfully deleted index.", body = [FileEntry])
    ),
    params(
        DeleteIndexQueryParam,
        ("index_id" = String, Path, description = "The index ID to delete."),
    )
)]
/// Deletes index.
async fn delete_index(
    index_id: String,
    delete_index_query_param: DeleteIndexQueryParam,
    mut index_service: IndexService,
) -> Result<Vec<SplitInfo>, IndexServiceError> {
    info!(index_id = %index_id, dry_run = delete_index_query_param.dry_run, "delete-index");
    index_service
        .delete_index(&index_id, delete_index_query_param.dry_run)
        .await
}

fn create_source_handler(
    index_service: IndexService,
) -> impl Filter<Extract = (impl warp::Reply,), Error = Rejection> + Clone {
    warp::path!("indexes" / String / "sources")
        .and(warp::post())
        .and(config_format_filter())
        .and(warp::body::content_length_limit(1024 * 1024))
        .and(warp::filters::body::bytes())
        .and(with_arg(index_service))
        .then(create_source)
        .and(extract_format_from_qs())
        .map(make_json_api_response)
}

#[utoipa::path(
    post,
    tag = "Sources",
    path = "/indexes/{index_id}/sources",
    request_body = VersionedSourceConfig,
    responses(
        // We return `VersionedSourceConfig` as it's the serialized model view.
        (status = 200, description = "Successfully created source.", body = VersionedSourceConfig)
    ),
    params(
        ("index_id" = String, Path, description = "The index ID to create a source for."),
    )
)]
/// Creates Source.
async fn create_source(
    index_id: String,
    config_format: ConfigFormat,
    source_config_bytes: Bytes,
    mut index_service: IndexService,
) -> Result<SourceConfig, IndexServiceError> {
    let source_config: SourceConfig =
        load_source_config_from_user_config(config_format, &source_config_bytes)
            .map_err(IndexServiceError::InvalidConfig)?;
    if let SourceParams::File(_) = &source_config.source_params {
        return Err(IndexServiceError::OperationNotAllowed(
            "file sources are limited to a local usage. please use the CLI command `quickwit tool \
             local-ingest` to ingest data from a file"
                .to_string(),
        ));
    }
    let index_metadata_request = IndexMetadataRequest::for_index_id(index_id.to_string());
    let index_uid: IndexUid = index_service
        .metastore()
        .index_metadata(index_metadata_request)
        .await?
        .deserialize_index_metadata()?
        .index_uid;
    info!(index_id = %index_id, source_id = %source_config.source_id, "create-source");
    index_service.create_source(index_uid, source_config).await
}

fn get_source_handler(
    metastore: MetastoreServiceClient,
) -> impl Filter<Extract = (impl warp::Reply,), Error = Rejection> + Clone {
    warp::path!("indexes" / String / "sources" / String)
        .and(warp::get())
        .and(with_arg(metastore))
        .then(get_source)
        .and(extract_format_from_qs())
        .map(make_json_api_response)
}

async fn get_source(
    index_id: String,
    source_id: String,
    mut metastore: MetastoreServiceClient,
) -> MetastoreResult<SourceConfig> {
    info!(index_id = %index_id, source_id = %source_id, "get-source");
    let index_metadata_request = IndexMetadataRequest::for_index_id(index_id.to_string());
    let source_config = metastore
        .index_metadata(index_metadata_request)
        .await?
        .deserialize_index_metadata()?
        .sources
        .remove(&source_id)
        .ok_or({
            MetastoreError::NotFound(EntityKind::Source {
                index_id,
                source_id,
            })
        })?;
    Ok(source_config)
}

fn reset_source_checkpoint_handler(
    metastore: MetastoreServiceClient,
) -> impl Filter<Extract = (impl warp::Reply,), Error = Rejection> + Clone {
    warp::path!("indexes" / String / "sources" / String / "reset-checkpoint")
        .and(warp::put())
        .and(with_arg(metastore))
        .then(reset_source_checkpoint)
        .and(extract_format_from_qs())
        .map(make_json_api_response)
}

#[utoipa::path(
    put,
    tag = "Sources",
    path = "/indexes/{index_id}/sources/{source_id}/reset-checkpoint",
    responses(
        (status = 200, description = "Successfully reset source checkpoint.")
    ),
    params(
        ("index_id" = String, Path, description = "The index ID of the source."),
        ("source_id" = String, Path, description = "The source ID whose checkpoint is reset."),
    )
)]
/// Resets source checkpoint.
async fn reset_source_checkpoint(
    index_id: String,
    source_id: String,
    mut metastore: MetastoreServiceClient,
) -> MetastoreResult<()> {
    let index_metadata_resquest = IndexMetadataRequest::for_index_id(index_id.to_string());
    let index_uid: IndexUid = metastore
        .index_metadata(index_metadata_resquest)
        .await?
        .deserialize_index_metadata()?
        .index_uid;
    info!(index_id = %index_id, source_id = %source_id, "reset-checkpoint");
    let reset_source_checkpoint_request = ResetSourceCheckpointRequest {
        index_uid: index_uid.to_string(),
        source_id: source_id.clone(),
    };
    metastore
        .reset_source_checkpoint(reset_source_checkpoint_request)
        .await?;
    Ok(())
}

fn toggle_source_handler(
    metastore: MetastoreServiceClient,
) -> impl Filter<Extract = (impl warp::Reply,), Error = Rejection> + Clone {
    warp::path!("indexes" / String / "sources" / String / "toggle")
        .and(warp::put())
        .and(json_body())
        .and(with_arg(metastore))
        .then(toggle_source)
        .and(extract_format_from_qs())
        .map(make_json_api_response)
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct ToggleSource {
    enable: bool,
}

#[utoipa::path(
    put,
    tag = "Sources",
    path = "/indexes/{index_id}/sources/{source_id}/toggle",
    request_body = ToggleSource,
    responses(
        (status = 200, description = "Successfully toggled source.")
    ),
    params(
        ("index_id" = String, Path, description = "The index ID of the source."),
        ("source_id" = String, Path, description = "The source ID to toggle."),
    )
)]
/// Toggles source.
async fn toggle_source(
    index_id: String,
    source_id: String,
    toggle_source: ToggleSource,
    mut metastore: MetastoreServiceClient,
) -> Result<(), IndexServiceError> {
    info!(index_id = %index_id, source_id = %source_id, enable = toggle_source.enable, "toggle-source");
    let index_metadata_request = IndexMetadataRequest::for_index_id(index_id.to_string());
    let index_uid: IndexUid = metastore
        .index_metadata(index_metadata_request)
        .await?
        .deserialize_index_metadata()?
        .index_uid;
    if [CLI_INGEST_SOURCE_ID, INGEST_API_SOURCE_ID].contains(&source_id.as_str()) {
        return Err(IndexServiceError::OperationNotAllowed(format!(
            "source `{source_id}` is managed by Quickwit, you cannot enable or disable a source \
             managed by Quickwit"
        )));
    }
    let toggle_source_request = ToggleSourceRequest {
        index_uid: index_uid.to_string(),
        source_id: source_id.clone(),
        enable: toggle_source.enable,
    };
    metastore.toggle_source(toggle_source_request).await?;
    Ok(())
}

fn delete_source_handler(
    metastore: MetastoreServiceClient,
) -> impl Filter<Extract = (impl warp::Reply,), Error = Rejection> + Clone {
    warp::path!("indexes" / String / "sources" / String)
        .and(warp::delete())
        .and(with_arg(metastore))
        .then(delete_source)
        .and(extract_format_from_qs())
        .map(make_json_api_response)
}

#[utoipa::path(
    delete,
    tag = "Sources",
    path = "/indexes/{index_id}/sources/{source_id}",
    responses(
        (status = 200, description = "Successfully deleted source.")
    ),
    params(
        ("index_id" = String, Path, description = "The index ID to remove the source from."),
        ("source_id" = String, Path, description = "The source ID to remove from the index."),
    )
)]
/// Deletes source.
async fn delete_source(
    index_id: String,
    source_id: String,
    mut metastore: MetastoreServiceClient,
) -> Result<(), IndexServiceError> {
    info!(index_id = %index_id, source_id = %source_id, "delete-source");
    let index_metadata_request = IndexMetadataRequest::for_index_id(index_id.to_string());
    let index_uid: IndexUid = metastore
        .index_metadata(index_metadata_request)
        .await?
        .deserialize_index_metadata()?
        .index_uid;
    if [INGEST_API_SOURCE_ID, CLI_INGEST_SOURCE_ID].contains(&source_id.as_str()) {
        return Err(IndexServiceError::OperationNotAllowed(format!(
            "source `{source_id}` is managed by Quickwit, you cannot delete a source managed by \
             Quickwit"
        )));
    }
    let delete_source_request = DeleteSourceRequest {
        index_uid: index_uid.to_string(),
        source_id: source_id.clone(),
    };
    metastore.delete_source(delete_source_request).await?;
    Ok(())
}

#[derive(Debug, Deserialize, utoipa::IntoParams, utoipa::ToSchema)]
struct AnalyzeRequest {
    /// The tokenizer to use.
    #[serde(flatten)]
    pub tokenizer_config: TokenizerConfig,
    /// The text to analyze.
    pub text: String,
}

fn analyze_request_filter() -> impl Filter<Extract = (AnalyzeRequest,), Error = Rejection> + Clone {
    warp::path!("analyze")
        .and(warp::post())
        .and(warp::body::json())
}

fn analyze_request_handler() -> impl Filter<Extract = (impl warp::Reply,), Error = Rejection> + Clone
{
    analyze_request_filter()
        .then(analyze_request)
        .and(extract_format_from_qs())
        .map(make_json_api_response)
}

/// Analyzes text with given tokenizer config and returns the list of tokens.
#[utoipa::path(
    post,
    tag = "analyze",
    path = "/analyze",
    request_body = AnalyzeRequest,
    responses(
        (status = 200, description = "Successfully analyze text.")
    ),
)]
async fn analyze_request(request: AnalyzeRequest) -> Result<serde_json::Value, IndexServiceError> {
    let tokens = analyze_text(&request.text, &request.tokenizer_config)
        .map_err(|err| IndexServiceError::Internal(format!("{err:?}")))?;
    let json_value = serde_json::to_value(tokens)
        .map_err(|err| IndexServiceError::Internal(format!("cannot serialize tokens: {err}")))?;
    Ok(json_value)
}

#[cfg(test)]
mod tests {
    use std::ops::{Bound, RangeInclusive};

    use assert_json_diff::assert_json_include;
    use quickwit_common::uri::Uri;
    use quickwit_config::{SourceParams, VecSourceParams};
    use quickwit_indexing::{mock_split, MockSplitBuilder};
    use quickwit_metastore::{metastore_for_test, IndexMetadata};
    use quickwit_proto::metastore::{
        EmptyResponse, IndexMetadataResponse, ListIndexesMetadataResponse, ListSplitsResponse,
        MetastoreServiceClient, SourceType,
    };
    use quickwit_storage::StorageResolver;
    use serde_json::Value as JsonValue;

    use super::*;
    use crate::recover_fn;

    #[tokio::test]
    async fn test_get_index() -> anyhow::Result<()> {
        let mut mock_metastore = MetastoreServiceClient::mock();
        mock_metastore.expect_index_metadata().return_once(|_| {
            Ok(
                IndexMetadataResponse::try_from_index_metadata(IndexMetadata::for_test(
                    "test-index",
                    "ram:///indexes/test-index",
                ))
                .unwrap(),
            )
        });
        let index_service = IndexService::new(
            MetastoreServiceClient::from(mock_metastore),
            StorageResolver::unconfigured(),
        );
        let index_management_handler =
            super::index_management_handlers(index_service, Arc::new(NodeConfig::for_test()))
                .recover(recover_fn);
        let resp = warp::test::request()
            .path("/indexes/test-index")
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 200);
        let actual_response_json: JsonValue = serde_json::from_slice(resp.body())?;
        let expected_response_json = serde_json::json!({
            "index_id": "test-index",
            "index_uri": "ram:///indexes/test-index",
        });
        assert_json_include!(
            actual: actual_response_json.get("index_config").unwrap(),
            expected: expected_response_json
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_get_non_existing_index() {
        let metastore = metastore_for_test();
        let index_service = IndexService::new(metastore, StorageResolver::unconfigured());
        let index_management_handler =
            super::index_management_handlers(index_service, Arc::new(NodeConfig::for_test()))
                .recover(recover_fn);
        let resp = warp::test::request()
            .path("/indexes/test-index")
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn test_get_splits() {
        let mut metastore = MetastoreServiceClient::mock();
        let index_metadata =
            IndexMetadata::for_test("quickwit-demo-index", "ram:///indexes/quickwit-demo-index");
        let index_uid = index_metadata.index_uid.clone();
        metastore
            .expect_index_metadata()
            .returning(move |_| {
                Ok(IndexMetadataResponse::try_from_index_metadata(index_metadata.clone()).unwrap())
            })
            .times(2);
        metastore
            .expect_list_splits()
            .returning(move |list_splits_request: ListSplitsRequest| {
                let list_split_query = list_splits_request.deserialize_list_splits_query().unwrap();
                if list_split_query.index_uids.contains(&index_uid)
                    && list_split_query.split_states
                        == vec![SplitState::Published, SplitState::Staged]
                    && list_split_query.time_range.start == Bound::Included(10)
                    && list_split_query.time_range.end == Bound::Excluded(20)
                    && list_split_query.create_timestamp.end == Bound::Excluded(2)
                {
                    let splits = vec![MockSplitBuilder::new("split_1")
                        .with_index_uid(&index_uid)
                        .build()];
                    return Ok(ListSplitsResponse::try_from_splits(splits).unwrap());
                }
                Err(MetastoreError::Internal {
                    message: "".to_string(),
                    cause: "".to_string(),
                })
            })
            .times(2);
        let index_service = IndexService::new(
            MetastoreServiceClient::from(metastore),
            StorageResolver::unconfigured(),
        );
        let index_management_handler =
            super::index_management_handlers(index_service, Arc::new(NodeConfig::for_test()))
                .recover(recover_fn);
        {
            let resp = warp::test::request()
                .path(
                    "/indexes/quickwit-demo-index/splits?split_states=Published,Staged&\
                     start_timestamp=10&end_timestamp=20&end_create_timestamp=2",
                )
                .reply(&index_management_handler)
                .await;
            assert_eq!(resp.status(), 200);
            let actual_response_json: JsonValue = serde_json::from_slice(resp.body()).unwrap();
            let expected_response_json = serde_json::json!({
                "splits": [
                    {
                        "create_timestamp": 0,
                        "split_id": "split_1",
                    }
                ]
            });
            assert_json_include!(
                actual: actual_response_json,
                expected: expected_response_json
            );
        }
        {
            let resp = warp::test::request()
                .path(
                    "/indexes/quickwit-demo-index/splits?split_states=Published&\
                     start_timestamp=11&end_timestamp=20&end_create_timestamp=2",
                )
                .reply(&index_management_handler)
                .await;
            assert_eq!(resp.status(), 500);
        }
    }

    #[tokio::test]
    async fn test_describe_index() -> anyhow::Result<()> {
        let mut mock_metastore = MetastoreServiceClient::mock();
        let index_metadata =
            IndexMetadata::for_test("quickwit-demo-index", "ram:///indexes/quickwit-demo-index");
        let index_uid = index_metadata.index_uid.clone();
        mock_metastore
            .expect_index_metadata()
            .return_once(move |_| {
                Ok(IndexMetadataResponse::try_from_index_metadata(index_metadata).unwrap())
            });
        let split_1 = MockSplitBuilder::new("split_1")
            .with_index_uid(&index_uid)
            .build();
        let split_1_time_range = split_1.split_metadata.time_range.clone().unwrap();
        let mut split_2 = MockSplitBuilder::new("split_2")
            .with_index_uid(&index_uid)
            .build();
        split_2.split_metadata.time_range = Some(RangeInclusive::new(
            split_1_time_range.start() - 10,
            split_1_time_range.end() + 10,
        ));
        mock_metastore
            .expect_list_splits()
            .withf(move |list_split_request| -> bool {
                let list_split_query = list_split_request.deserialize_list_splits_query().unwrap();
                list_split_query.index_uids.contains(&index_uid)
            })
            .return_once(move |_| {
                let splits = vec![split_1, split_2];
                Ok(ListSplitsResponse::try_from_splits(splits).unwrap())
            });

        let index_service = IndexService::new(
            MetastoreServiceClient::from(mock_metastore),
            StorageResolver::unconfigured(),
        );
        let index_management_handler =
            super::index_management_handlers(index_service, Arc::new(NodeConfig::for_test()))
                .recover(recover_fn);
        let resp = warp::test::request()
            .path("/indexes/quickwit-demo-index/describe")
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 200);

        let actual_response_json: JsonValue = serde_json::from_slice(resp.body()).unwrap();
        let expected_response_json = serde_json::json!({
            "index_id": "quickwit-demo-index",
            "index_uri": "ram:///indexes/quickwit-demo-index",
            "num_published_splits": 2,
            "size_published_splits": 1600,
            "num_published_docs": 20,
            "size_published_docs_uncompressed": 512,
            "timestamp_field_name": "timestamp",
            "min_timestamp": split_1_time_range.start() - 10,
            "max_timestamp": split_1_time_range.end() + 10,
        });

        assert_eq!(actual_response_json, expected_response_json);
        Ok(())
    }

    #[tokio::test]
    async fn test_get_all_splits() {
        let mut mock_metastore = MetastoreServiceClient::mock();
        let index_metadata =
            IndexMetadata::for_test("quickwit-demo-index", "ram:///indexes/quickwit-demo-index");
        let index_uid = index_metadata.index_uid.clone();
        mock_metastore
            .expect_index_metadata()
            .return_once(move |_| {
                Ok(IndexMetadataResponse::try_from_index_metadata(index_metadata).unwrap())
            });
        mock_metastore.expect_list_splits().return_once(
            move |list_split_request: ListSplitsRequest| {
                let list_split_query = list_split_request.deserialize_list_splits_query().unwrap();
                if list_split_query.index_uids.contains(&index_uid)
                    && list_split_query.split_states.is_empty()
                    && list_split_query.time_range.is_unbounded()
                    && list_split_query.create_timestamp.is_unbounded()
                {
                    return Ok(ListSplitsResponse::empty());
                }
                Err(MetastoreError::Internal {
                    message: "".to_string(),
                    cause: "".to_string(),
                })
            },
        );
        let index_service = IndexService::new(
            MetastoreServiceClient::from(mock_metastore),
            StorageResolver::unconfigured(),
        );
        let index_management_handler =
            super::index_management_handlers(index_service, Arc::new(NodeConfig::for_test()))
                .recover(recover_fn);
        let resp = warp::test::request()
            .path("/indexes/quickwit-demo-index/splits")
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn test_mark_splits_for_deletion() -> anyhow::Result<()> {
        let mut mock_metastore = MetastoreServiceClient::mock();
        mock_metastore
            .expect_index_metadata()
            .returning(|_| {
                Ok(
                    IndexMetadataResponse::try_from_index_metadata(IndexMetadata::for_test(
                        "quickwit-demo-index",
                        "ram:///indexes/quickwit-demo-index",
                    ))
                    .unwrap(),
                )
            })
            .times(2);
        mock_metastore
            .expect_mark_splits_for_deletion()
            .returning(
                |mark_splits_for_deletion_request: MarkSplitsForDeletionRequest| {
                    let split_ids = mark_splits_for_deletion_request.split_ids;
                    let index_uid: IndexUid = mark_splits_for_deletion_request.index_uid.into();
                    if index_uid.index_id() == "quickwit-demo-index"
                        && split_ids == ["split-1", "split-2"]
                    {
                        return Ok(EmptyResponse {});
                    }
                    Err(MetastoreError::Internal {
                        message: "".to_string(),
                        cause: "".to_string(),
                    })
                },
            )
            .times(2);
        let index_service = IndexService::new(
            MetastoreServiceClient::from(mock_metastore),
            StorageResolver::unconfigured(),
        );
        let index_management_handler =
            super::index_management_handlers(index_service, Arc::new(NodeConfig::for_test()))
                .recover(recover_fn);
        let resp = warp::test::request()
            .path("/indexes/quickwit-demo-index/splits/mark-for-deletion")
            .method("PUT")
            .json(&true)
            .body(r#"{"split_ids": ["split-1", "split-2"]}"#)
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 200);
        let resp = warp::test::request()
            .path("/indexes/quickwit-demo-index/splits/mark-for-deletion")
            .json(&true)
            .body(r#"{"split_ids": [""]}"#)
            .method("PUT")
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 500);
        Ok(())
    }

    #[tokio::test]
    async fn test_get_list_indexes() -> anyhow::Result<()> {
        let mut mock_metastore = MetastoreServiceClient::mock();
        mock_metastore
            .expect_list_indexes_metadata()
            .return_once(|_list_indexes_request| {
                let index_metadata =
                    IndexMetadata::for_test("test-index", "ram:///indexes/test-index");
                Ok(
                    ListIndexesMetadataResponse::try_from_indexes_metadata(vec![index_metadata])
                        .unwrap(),
                )
            });
        let index_service = IndexService::new(
            MetastoreServiceClient::from(mock_metastore),
            StorageResolver::unconfigured(),
        );
        let index_management_handler =
            super::index_management_handlers(index_service, Arc::new(NodeConfig::for_test()))
                .recover(recover_fn);
        let resp = warp::test::request()
            .path("/indexes")
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 200);
        let actual_response_json: JsonValue = serde_json::from_slice(resp.body())?;
        let actual_response_arr: &Vec<JsonValue> = actual_response_json.as_array().unwrap();
        assert_eq!(actual_response_arr.len(), 1);
        let actual_index_metadata_json: &JsonValue = &actual_response_arr[0];
        let expected_response_json = serde_json::json!({
            "index_id": "test-index",
            "index_uri": "ram:///indexes/test-index",
        });
        assert_json_include!(
            actual: actual_index_metadata_json.get("index_config").unwrap(),
            expected: expected_response_json
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_clear_index() -> anyhow::Result<()> {
        let mut mock_metastore = MetastoreServiceClient::mock();
        mock_metastore.expect_index_metadata().return_once(|_| {
            Ok(
                IndexMetadataResponse::try_from_index_metadata(IndexMetadata::for_test(
                    "quickwit-demo-index",
                    "file:///path/to/index/quickwit-demo-index",
                ))
                .unwrap(),
            )
        });
        mock_metastore.expect_list_splits().return_once(|_| {
            Ok(ListSplitsResponse::try_from_splits(vec![mock_split("split_1")]).unwrap())
        });
        mock_metastore
            .expect_mark_splits_for_deletion()
            .return_once(|_| Ok(EmptyResponse {}));
        mock_metastore
            .expect_delete_splits()
            .return_once(|_| Ok(EmptyResponse {}));
        mock_metastore
            .expect_reset_source_checkpoint()
            .return_once(|_| Ok(EmptyResponse {}));
        let index_service = IndexService::new(
            MetastoreServiceClient::from(mock_metastore),
            StorageResolver::unconfigured(),
        );
        let index_management_handler =
            super::index_management_handlers(index_service, Arc::new(NodeConfig::for_test()))
                .recover(recover_fn);
        let resp = warp::test::request()
            .path("/indexes/quickwit-demo-index/clear")
            .method("PUT")
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 200);
        Ok(())
    }

    #[tokio::test]
    async fn test_delete_index() {
        let mut mock_metastore = MetastoreServiceClient::mock();
        mock_metastore
            .expect_index_metadata()
            .returning(|_| {
                Ok(
                    IndexMetadataResponse::try_from_index_metadata(IndexMetadata::for_test(
                        "quickwit-demo-index",
                        "file:///path/to/index/quickwit-demo-index",
                    ))
                    .unwrap(),
                )
            })
            .times(2);
        mock_metastore
            .expect_list_splits()
            .returning(|_| {
                Ok(ListSplitsResponse::try_from_splits(vec![mock_split("split_1")]).unwrap())
            })
            .times(3);
        mock_metastore
            .expect_mark_splits_for_deletion()
            .return_once(|_| Ok(EmptyResponse {}));
        mock_metastore
            .expect_delete_splits()
            .return_once(|_| Ok(EmptyResponse {}));
        mock_metastore
            .expect_delete_index()
            .return_once(|_| Ok(EmptyResponse {}));
        let index_service = IndexService::new(
            MetastoreServiceClient::from(mock_metastore),
            StorageResolver::unconfigured(),
        );
        let index_management_handler =
            super::index_management_handlers(index_service, Arc::new(NodeConfig::for_test()))
                .recover(recover_fn);
        {
            // Dry run
            let resp = warp::test::request()
                .path("/indexes/quickwit-demo-index?dry_run=true")
                .method("DELETE")
                .reply(&index_management_handler)
                .await;
            assert_eq!(resp.status(), 200);
            let resp_json: serde_json::Value = serde_json::from_slice(resp.body()).unwrap();
            let expected_response_json = serde_json::json!([{
                "file_name": "split_1.split",
                "file_size_bytes": "800 B",
            }]);
            assert_json_include!(actual: resp_json, expected: expected_response_json);
        }
        {
            let resp = warp::test::request()
                .path("/indexes/quickwit-demo-index")
                .method("DELETE")
                .reply(&index_management_handler)
                .await;
            assert_eq!(resp.status(), 200);
            let resp_json: serde_json::Value = serde_json::from_slice(resp.body()).unwrap();
            let expected_response_json = serde_json::json!([{
                "file_name": "split_1.split",
                "file_size_bytes": "800 B",
            }]);
            assert_json_include!(actual: resp_json, expected: expected_response_json);
        }
    }

    #[tokio::test]
    async fn test_delete_on_non_existing_index() {
        let metastore = metastore_for_test();
        let index_service = IndexService::new(metastore, StorageResolver::unconfigured());
        let index_management_handler =
            super::index_management_handlers(index_service, Arc::new(NodeConfig::for_test()))
                .recover(recover_fn);
        let resp = warp::test::request()
            .path("/indexes/quickwit-demo-index")
            .method("DELETE")
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn test_create_index_with_overwrite() {
        let metastore = metastore_for_test();
        let index_service = IndexService::new(metastore.clone(), StorageResolver::unconfigured());
        let mut node_config = NodeConfig::for_test();
        node_config.default_index_root_uri = Uri::for_test("file:///default-index-root-uri");
        let index_management_handler =
            super::index_management_handlers(index_service, Arc::new(node_config));
        {
            let resp = warp::test::request()
                .path("/indexes?overwrite=true")
                .method("POST")
                .json(&true)
                .body(r#"{"version": "0.6", "index_id": "hdfs-logs", "doc_mapping": {"field_mappings":[{"name": "timestamp", "type": "i64", "fast": true, "indexed": true}]}}"#)
                .reply(&index_management_handler)
                .await;
            assert_eq!(resp.status(), 200);
        }
        {
            let resp = warp::test::request()
                .path("/indexes?overwrite=true")
                .method("POST")
                .json(&true)
                .body(r#"{"version": "0.6", "index_id": "hdfs-logs", "doc_mapping": {"field_mappings":[{"name": "timestamp", "type": "i64", "fast": true, "indexed": true}]}}"#)
                .reply(&index_management_handler)
                .await;
            assert_eq!(resp.status(), 200);
        }
        {
            let resp = warp::test::request()
                .path("/indexes")
                .method("POST")
                .json(&true)
                .body(r#"{"version": "0.6", "index_id": "hdfs-logs", "doc_mapping": {"field_mappings":[{"name": "timestamp", "type": "i64", "fast": true, "indexed": true}]}}"#)
                .reply(&index_management_handler)
                .await;
            assert_eq!(resp.status(), 400);
        }
    }

    #[tokio::test]
    async fn test_create_delete_index_and_source() {
        let mut metastore = metastore_for_test();
        let index_service = IndexService::new(metastore.clone(), StorageResolver::unconfigured());
        let mut node_config = NodeConfig::for_test();
        node_config.default_index_root_uri = Uri::for_test("file:///default-index-root-uri");
        let index_management_handler =
            super::index_management_handlers(index_service, Arc::new(node_config));
        let resp = warp::test::request()
            .path("/indexes")
            .method("POST")
            .json(&true)
            .body(r#"{"version": "0.6", "index_id": "hdfs-logs", "doc_mapping": {"field_mappings":[{"name": "timestamp", "type": "i64", "fast": true, "indexed": true}]}}"#)
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 200);
        let resp_json: serde_json::Value = serde_json::from_slice(resp.body()).unwrap();
        let expected_response_json = serde_json::json!({
            "index_config": {
                "index_id": "hdfs-logs",
                "index_uri": "file:///default-index-root-uri/hdfs-logs",
            }
        });
        assert_json_include!(actual: resp_json, expected: expected_response_json);

        // Create source.
        let source_config_body = r#"{"version": "0.6", "source_id": "vec-source", "source_type": "vec", "params": {"docs": [], "batch_num_docs": 10}}"#;
        let resp = warp::test::request()
            .path("/indexes/hdfs-logs/sources")
            .method("POST")
            .json(&true)
            .body(source_config_body)
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 200);

        // Get source.
        let resp = warp::test::request()
            .path("/indexes/hdfs-logs/sources/vec-source")
            .method("GET")
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 200);

        // Check that the source has been added to index metadata.
        let index_metadata = metastore
            .index_metadata(IndexMetadataRequest::for_index_id("hdfs-logs".to_string()))
            .await
            .unwrap()
            .deserialize_index_metadata()
            .unwrap();
        assert!(index_metadata.sources.contains_key("vec-source"));
        let source_config = index_metadata.sources.get("vec-source").unwrap();
        assert_eq!(source_config.source_type(), SourceType::Vec);
        assert_eq!(
            source_config.source_params,
            SourceParams::Vec(VecSourceParams {
                docs: Vec::new(),
                batch_num_docs: 10,
                partition: "".to_string(),
            })
        );

        // Check delete source.
        let resp = warp::test::request()
            .path("/indexes/hdfs-logs/sources/vec-source")
            .method("DELETE")
            .body(source_config_body)
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 200);
        let index_metadata = metastore
            .index_metadata(IndexMetadataRequest::for_index_id("hdfs-logs".to_string()))
            .await
            .unwrap()
            .deserialize_index_metadata()
            .unwrap();
        assert!(!index_metadata.sources.contains_key("file-source"));

        // Check cannot delete source managed by Quickwit.
        let resp = warp::test::request()
            .path(format!("/indexes/hdfs-logs/sources/{INGEST_API_SOURCE_ID}").as_str())
            .method("DELETE")
            .body(source_config_body)
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 405);

        let resp = warp::test::request()
            .path(format!("/indexes/hdfs-logs/sources/{CLI_INGEST_SOURCE_ID}").as_str())
            .method("DELETE")
            .body(source_config_body)
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 405);

        // Check get a non existing source returns 404.
        let resp = warp::test::request()
            .path("/indexes/hdfs-logs/sources/file-source")
            .method("GET")
            .body(source_config_body)
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 404);

        // Check delete index.
        let resp = warp::test::request()
            .path("/indexes/hdfs-logs")
            .method("DELETE")
            .body(source_config_body)
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 200);
        let indexes = metastore
            .list_indexes_metadata(ListIndexesMetadataRequest::all())
            .await
            .unwrap()
            .deserialize_indexes_metadata()
            .unwrap();
        assert!(indexes.is_empty());
    }

    #[tokio::test]
    async fn test_create_file_source_returns_405() {
        let metastore = metastore_for_test();
        let index_service = IndexService::new(metastore.clone(), StorageResolver::unconfigured());
        let mut node_config = NodeConfig::for_test();
        node_config.default_index_root_uri = Uri::for_test("file:///default-index-root-uri");
        let index_management_handler =
            super::index_management_handlers(index_service, Arc::new(node_config))
                .recover(recover_fn);
        let source_config_body = r#"{"version": "0.6", "source_id": "file-source", "source_type":
    "file", "params": {"filepath": "FILEPATH"}}"#;
        let resp = warp::test::request()
            .path("/indexes/hdfs-logs/sources")
            .method("POST")
            .body(source_config_body)
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 405);
        let response_body = std::str::from_utf8(resp.body()).unwrap();
        assert!(response_body.contains("limited to a local usage"))
    }

    #[tokio::test]
    async fn test_create_index_with_yaml() {
        let metastore = metastore_for_test();
        let index_service = IndexService::new(metastore.clone(), StorageResolver::unconfigured());
        let mut node_config = NodeConfig::for_test();
        node_config.default_index_root_uri = Uri::for_test("file:///default-index-root-uri");
        let index_management_handler =
            super::index_management_handlers(index_service, Arc::new(node_config))
                .recover(recover_fn);
        let resp = warp::test::request()
            .path("/indexes")
            .method("POST")
            .header("content-type", "application/yaml")
            .body(
                r#"
            version: 0.6
            index_id: hdfs-logs
            doc_mapping:
              field_mappings:
                - name: timestamp
                  type: i64
                  fast: true
                  indexed: true
            "#,
            )
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 200);
        let resp_json: serde_json::Value = serde_json::from_slice(resp.body()).unwrap();
        let expected_response_json = serde_json::json!({
            "index_config": {
                "index_id": "hdfs-logs",
                "index_uri": "file:///default-index-root-uri/hdfs-logs",
            }
        });
        assert_json_include!(actual: resp_json, expected: expected_response_json);
    }

    #[tokio::test]
    async fn test_create_index_and_source_with_toml() {
        let metastore = metastore_for_test();
        let index_service = IndexService::new(metastore.clone(), StorageResolver::unconfigured());
        let mut node_config = NodeConfig::for_test();
        node_config.default_index_root_uri = Uri::for_test("file:///default-index-root-uri");
        let index_management_handler =
            super::index_management_handlers(index_service, Arc::new(node_config))
                .recover(recover_fn);
        let resp = warp::test::request()
            .path("/indexes")
            .method("POST")
            .header("content-type", "application/toml")
            .body(
                r#"
            version = "0.6"
            index_id = "hdfs-logs"
            [doc_mapping]
            field_mappings = [
                { name = "timestamp", type = "i64", fast = true, indexed = true}
            ]
            "#,
            )
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 200);
        let resp_json: serde_json::Value = serde_json::from_slice(resp.body()).unwrap();
        let expected_response_json = serde_json::json!({
            "index_config": {
                "index_id": "hdfs-logs",
                "index_uri": "file:///default-index-root-uri/hdfs-logs",
            }
        });
        assert_json_include!(actual: resp_json, expected: expected_response_json);
    }

    #[tokio::test]
    async fn test_create_index_with_wrong_content_type() {
        let metastore = metastore_for_test();
        let index_service = IndexService::new(metastore.clone(), StorageResolver::unconfigured());
        let mut node_config = NodeConfig::for_test();
        node_config.default_index_root_uri = Uri::for_test("file:///default-index-root-uri");
        let index_management_handler =
            super::index_management_handlers(index_service, Arc::new(node_config))
                .recover(recover_fn);
        let resp = warp::test::request()
            .path("/indexes")
            .method("POST")
            .header("content-type", "application/yoml")
            .body(r#""#)
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 415);
        let body = std::str::from_utf8(resp.body()).unwrap();
        assert!(body.contains("unsupported content-type header. choices are"));
    }

    #[tokio::test]
    async fn test_create_index_with_bad_config() -> anyhow::Result<()> {
        let metastore = MetastoreServiceClient::mock();
        let index_service = IndexService::new(
            MetastoreServiceClient::from(metastore),
            StorageResolver::unconfigured(),
        );
        let index_management_handler =
            super::index_management_handlers(index_service, Arc::new(NodeConfig::for_test()))
                .recover(recover_fn);
        let resp = warp::test::request()
            .path("/indexes")
            .method("POST")
            .json(&true)
            .body(
                r#"{"version": "0.6", "index_id": "hdfs-log", "doc_mapping":
    {"field_mappings":[{"name": "timestamp", "type": "unknown", "fast": true, "indexed":
    true}]}}"#,
            )
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 400);
        let body = std::str::from_utf8(resp.body()).unwrap();
        assert!(body.contains("field `timestamp` has an unknown type"));
        Ok(())
    }

    #[tokio::test]
    async fn test_create_source_with_bad_config() {
        let metastore = metastore_for_test();
        let index_service = IndexService::new(metastore, StorageResolver::unconfigured());
        let index_management_handler =
            super::index_management_handlers(index_service, Arc::new(NodeConfig::for_test()))
                .recover(recover_fn);
        {
            // Source config with bad version.
            let resp = warp::test::request()
                .path("/indexes/my-index/sources")
                .method("POST")
                .json(&true)
                .body(r#"{"version": 0.4, "source_id": "file-source"}"#)
                .reply(&index_management_handler)
                .await;
            assert_eq!(resp.status(), 400);
            let body = std::str::from_utf8(resp.body()).unwrap();
            assert!(body.contains("invalid type: floating point `0.4`"));
        }
        {
            // Invalid pulsar source config with number of pipelines > 1, not supported yet.
            let resp = warp::test::request()
                .path("/indexes/my-index/sources")
                .method("POST")
                .json(&true)
                .body(
                    r#"{"version": "0.6", "source_id": "pulsar-source",
    "desired_num_pipelines": 2, "source_type": "pulsar", "params": {"topics": ["my-topic"],
    "address": "pulsar://localhost:6650" }}"#,
                )
                .reply(&index_management_handler)
                .await;
            assert_eq!(resp.status(), 400);
            let body = std::str::from_utf8(resp.body()).unwrap();
            assert!(body.contains(
                "Quickwit currently supports multiple pipelines only for GCP PubSub or Kafka \
                 sources"
            ));
        }
    }

    #[tokio::test]
    async fn test_delete_non_existing_source() {
        let mut mock_metastore = MetastoreServiceClient::mock();
        mock_metastore.expect_index_metadata().return_once(|_| {
            Ok(
                IndexMetadataResponse::try_from_index_metadata(IndexMetadata::for_test(
                    "quickwit-demo-index",
                    "file:///path/to/index/quickwit-demo-index",
                ))
                .unwrap(),
            )
        });
        // TODO
        // metastore
        //     .expect_index_exists()
        //     .return_once(|index_id: &str| Ok(index_id == "quickwit-demo-index"));
        mock_metastore.expect_delete_source().return_once(
            |delete_source_request: DeleteSourceRequest| {
                let source_id = delete_source_request.source_id;
                let index_uid: IndexUid = delete_source_request.index_uid.into();
                assert_eq!(index_uid.index_id(), "quickwit-demo-index");
                Err(MetastoreError::NotFound(EntityKind::Source {
                    index_id: "quickwit-demo-index".to_string(),
                    source_id: source_id.to_string(),
                }))
            },
        );
        let index_service = IndexService::new(
            MetastoreServiceClient::from(mock_metastore),
            StorageResolver::unconfigured(),
        );
        let index_management_handler =
            super::index_management_handlers(index_service, Arc::new(NodeConfig::for_test()))
                .recover(recover_fn);
        let resp = warp::test::request()
            .path("/indexes/quickwit-demo-index/sources/foo-source")
            .method("DELETE")
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn test_source_reset_checkpoint() -> anyhow::Result<()> {
        let mut mock_metastore = MetastoreServiceClient::mock();
        mock_metastore
            .expect_index_metadata()
            .returning(|_| {
                Ok(
                    IndexMetadataResponse::try_from_index_metadata(IndexMetadata::for_test(
                        "quickwit-demo-index",
                        "file:///path/to/index/quickwit-demo-index",
                    ))
                    .unwrap(),
                )
            })
            .times(2);
        mock_metastore
            .expect_reset_source_checkpoint()
            .returning(
                |reset_source_checkpoint_request: ResetSourceCheckpointRequest| {
                    let source_id = reset_source_checkpoint_request.source_id;
                    let index_uid: IndexUid = reset_source_checkpoint_request.index_uid.into();
                    if index_uid.index_id() == "quickwit-demo-index"
                        && source_id == "source-to-reset"
                    {
                        return Ok(EmptyResponse {});
                    }
                    Err(MetastoreError::Internal {
                        message: "".to_string(),
                        cause: "".to_string(),
                    })
                },
            )
            .times(2);
        let index_service = IndexService::new(
            MetastoreServiceClient::from(mock_metastore),
            StorageResolver::unconfigured(),
        );
        let index_management_handler =
            super::index_management_handlers(index_service, Arc::new(NodeConfig::for_test()))
                .recover(recover_fn);
        let resp = warp::test::request()
            .path("/indexes/quickwit-demo-index/sources/source-to-reset/reset-checkpoint")
            .method("PUT")
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 200);
        let resp = warp::test::request()
            .path("/indexes/quickwit-demo-index/sources/source-to-reset-2/reset-checkpoint")
            .method("PUT")
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 500);
        Ok(())
    }

    #[tokio::test]
    async fn test_source_toggle() -> anyhow::Result<()> {
        let mut mock_metastore = MetastoreServiceClient::mock();
        mock_metastore
            .expect_index_metadata()
            .returning(|_| {
                Ok(
                    IndexMetadataResponse::try_from_index_metadata(IndexMetadata::for_test(
                        "quickwit-demo-index",
                        "file:///path/to/index/quickwit-demo-index",
                    ))
                    .unwrap(),
                )
            })
            .times(3);
        mock_metastore.expect_toggle_source().return_once(
            |toggle_source_request: ToggleSourceRequest| {
                let source_id = toggle_source_request.source_id;
                let index_uid: IndexUid = toggle_source_request.index_uid.into();
                let enable = toggle_source_request.enable;
                if index_uid.index_id() == "quickwit-demo-index"
                    && source_id == "source-to-toggle"
                    && enable
                {
                    return Ok(EmptyResponse {});
                }
                Err(MetastoreError::Internal {
                    message: "".to_string(),
                    cause: "".to_string(),
                })
            },
        );
        let index_service = IndexService::new(
            MetastoreServiceClient::from(mock_metastore),
            StorageResolver::unconfigured(),
        );
        let index_management_handler =
            super::index_management_handlers(index_service, Arc::new(NodeConfig::for_test()))
                .recover(recover_fn);
        // Check server returns 405 if sources root path is used.
        let resp = warp::test::request()
            .path("/indexes/quickwit-demo-index/sources/source-to-toggle")
            .method("PUT")
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 405);
        let resp = warp::test::request()
            .path("/indexes/quickwit-demo-index/sources/source-to-toggle/toggle")
            .method("PUT")
            .json(&true)
            .body(r#"{"enable": true}"#)
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 200);
        let resp = warp::test::request()
            .path("/indexes/quickwit-demo-index/sources/source-to-toggle/toggle")
            .method("PUT")
            .json(&true)
            .body(r#"{"toggle": true}"#) // unknown field, should return 400.
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 400);
        // Check cannot toggle source managed by Quickwit.
        let resp = warp::test::request()
            .path(format!("/indexes/hdfs-logs/sources/{INGEST_API_SOURCE_ID}/toggle").as_str())
            .method("PUT")
            .body(r#"{"enable": true}"#)
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 405);

        let resp = warp::test::request()
            .path(format!("/indexes/hdfs-logs/sources/{CLI_INGEST_SOURCE_ID}/toggle").as_str())
            .method("PUT")
            .body(r#"{"enable": true}"#)
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 405);
        Ok(())
    }

    #[tokio::test]
    async fn test_analyze_request() {
        let mut metastore = MetastoreServiceClient::mock();
        metastore.expect_index_metadata().return_once(|_| {
            Ok(
                IndexMetadataResponse::try_from_index_metadata(IndexMetadata::for_test(
                    "test-index",
                    "ram:///indexes/test-index",
                ))
                .unwrap(),
            )
        });
        let index_service = IndexService::new(
            MetastoreServiceClient::from(metastore),
            StorageResolver::unconfigured(),
        );
        let index_management_handler =
            super::index_management_handlers(index_service, Arc::new(NodeConfig::for_test()))
                .recover(recover_fn);
        let resp = warp::test::request()
            .path("/analyze")
            .method("POST")
            .json(&true)
            .body(
                r#"{"type": "ngram", "min_gram": 3, "max_gram": 3, "text": "Hel", "filters":
    ["lower_caser"]}"#,
            )
            .reply(&index_management_handler)
            .await;
        assert_eq!(resp.status(), 200);
        let actual_response_json: JsonValue = serde_json::from_slice(resp.body()).unwrap();
        let expected_response_json = serde_json::json!([
            {
                "offset_from": 0,
                "offset_to": 3,
                "position": 0,
                "position_length": 1,
                "text": "hel"
            }
        ]);
        assert_json_include!(
            actual: actual_response_json,
            expected: expected_response_json
        );
    }
}
