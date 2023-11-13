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

mod fetch;
mod ingester;
mod models;
mod mrecord;
mod mrecordlog_utils;
mod rate_limiter;
mod replication;
mod router;
mod shard_table;
#[cfg(test)]
mod test_utils;
mod workbench;

use bytesize::ByteSize;
use quickwit_common::tower::Pool;
use quickwit_proto::ingest::ingester::IngesterServiceClient;
use quickwit_proto::ingest::DocBatchV2;
use quickwit_proto::types::NodeId;

pub use self::fetch::{FetchStreamError, MultiFetchStream};
pub use self::ingester::Ingester;
use self::mrecord::MRECORD_HEADER_LEN;
pub use self::mrecord::{decoded_mrecords, MRecord};
pub use self::rate_limiter::RateLimiterSettings;
pub use self::router::IngestRouter;

pub type IngesterPool = Pool<NodeId, IngesterServiceClient>;

/// Identifies an ingester client, typically a source, for logging and debugging purposes.
pub type ClientId = String;

pub type LeaderId = NodeId;

pub type FollowerId = NodeId;

pub(super) fn estimate_size(doc_batch: &DocBatchV2) -> ByteSize {
    let estimate = doc_batch.num_bytes() + doc_batch.num_docs() * MRECORD_HEADER_LEN;
    ByteSize(estimate as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_size() {
        let doc_batch = DocBatchV2 {
            doc_buffer: Vec::new().into(),
            doc_lengths: Vec::new(),
        };
        assert_eq!(estimate_size(&doc_batch), ByteSize(0));

        let doc_batch = DocBatchV2 {
            doc_buffer: vec![0u8; 100].into(),
            doc_lengths: vec![10, 20, 30],
        };
        assert_eq!(estimate_size(&doc_batch), ByteSize(106));
    }
}
