//! Block part of API implementation.

// Built-in uses
use std::str::FromStr;

// External uses
use actix_web::{web, Scope};

// Workspace uses
use zksync_api_types::v02::{
    block::{BlockInfo, BlockStatus},
    pagination::{BlockAndTxHash, Paginated, PaginationQuery},
    transaction::Transaction,
};
use zksync_crypto::{convert::FeConvert, Fr};
use zksync_storage::{chain::block::records::BlockDetails, ConnectionPool, QueryResult};
use zksync_types::{tx::TxHash, BlockNumber, H256};

// Local uses
use super::{
    error::{Error, InvalidDataError},
    paginate_trait::Paginate,
    response::ApiResult,
};
use crate::{api_try, utils::block_details_cache::BlockDetailsCache};

pub fn block_info_from_details_and_status(details: BlockDetails, status: BlockStatus) -> BlockInfo {
    let (committed_at, finalized_at) = match status {
        BlockStatus::Queued => (None, None),
        BlockStatus::Committed => (Some(details.committed_at), None),
        BlockStatus::Finalized => (Some(details.committed_at), details.verified_at),
    };
    BlockInfo {
        block_number: BlockNumber(details.block_number as u32),
        new_state_root: Fr::from_bytes(&details.new_state_root).unwrap_or_else(|err| {
            panic!(
                "Database provided an incorrect new_state_root field: {:?}, an error occurred {}",
                details.new_state_root, err
            )
        }),
        block_size: details.block_size as u64,
        commit_tx_hash: details.commit_tx_hash.map(|bytes| H256::from_slice(&bytes)),
        verify_tx_hash: details.verify_tx_hash.map(|bytes| H256::from_slice(&bytes)),
        committed_at,
        finalized_at,
        status,
    }
}

/// Shared data between `api/v0.2/block` endpoints.
#[derive(Debug, Clone)]
struct ApiBlockData {
    pool: ConnectionPool,
    verified_blocks_cache: BlockDetailsCache,
}

impl ApiBlockData {
    fn new(pool: ConnectionPool, verified_blocks_cache: BlockDetailsCache) -> Self {
        Self {
            pool,
            verified_blocks_cache,
        }
    }

    /// Returns information about block with the specified number.
    ///
    /// This method caches some of the verified blocks.
    async fn block_info(&self, block_number: BlockNumber) -> Result<Option<BlockInfo>, Error> {
        let status = {
            let mut storage = self.pool.access_storage().await.map_err(Error::storage)?;
            storage
                .chain()
                .block_schema()
                .get_block_status_and_last_updated(block_number)
                .await
                .map_err(Error::storage)?
                .0
        };
        let details = self
            .verified_blocks_cache
            .get(&self.pool, block_number)
            .await
            .map_err(Error::storage)?;
        Ok(details.map(|details| block_info_from_details_and_status(details, status)))
    }

    async fn get_block_number_by_position(
        &self,
        block_position: &str,
    ) -> Result<BlockNumber, Error> {
        if let Ok(number) = u32::from_str(block_position) {
            Ok(BlockNumber(number))
        } else {
            match block_position {
                "last_committed" => self
                    .get_last_committed_block_number()
                    .await
                    .map_err(Error::storage),
                "last_finalized" => self
                    .get_last_finalized_block_number()
                    .await
                    .map_err(Error::storage),
                _ => Err(Error::from(InvalidDataError::InvalidBlockPosition)),
            }
        }
    }

    async fn block_page(
        &self,
        query: PaginationQuery<BlockNumber>,
    ) -> Result<Paginated<BlockInfo, BlockNumber>, Error> {
        let mut storage = self.pool.access_storage().await.map_err(Error::storage)?;
        storage.paginate(&query).await
    }

    async fn transaction_page(
        &self,
        block_number: BlockNumber,
        query: PaginationQuery<TxHash>,
    ) -> Result<Paginated<Transaction, BlockAndTxHash>, Error> {
        let mut storage = self.pool.access_storage().await.map_err(Error::storage)?;

        let new_query = PaginationQuery {
            from: BlockAndTxHash {
                block_number,
                tx_hash: query.from,
            },
            limit: query.limit,
            direction: query.direction,
        };

        storage.paginate(&new_query).await
    }

    async fn get_last_committed_block_number(&self) -> QueryResult<BlockNumber> {
        let mut storage = self.pool.access_storage().await?;
        storage
            .chain()
            .block_schema()
            .get_last_committed_block()
            .await
    }

    async fn get_last_finalized_block_number(&self) -> QueryResult<BlockNumber> {
        let mut storage = self.pool.access_storage().await?;
        storage
            .chain()
            .block_schema()
            .get_last_verified_confirmed_block()
            .await
    }
}

// Server implementation

async fn block_pagination(
    data: web::Data<ApiBlockData>,
    web::Query(query): web::Query<PaginationQuery<BlockNumber>>,
) -> ApiResult<Paginated<BlockInfo, BlockNumber>> {
    data.block_page(query).await.into()
}

// TODO: take `block_position` as enum.
// Currently actix path extractor doesn't work with enums: https://github.com/actix/actix-web/issues/318 (ZKS-628)
async fn block_by_number(
    data: web::Data<ApiBlockData>,
    web::Path(block_position): web::Path<String>,
) -> ApiResult<Option<BlockInfo>> {
    let block_number = api_try!(data.get_block_number_by_position(&block_position).await);

    data.block_info(block_number).await.into()
}

async fn block_transactions(
    data: web::Data<ApiBlockData>,
    web::Path(block_position): web::Path<String>,
    web::Query(query): web::Query<PaginationQuery<TxHash>>,
) -> ApiResult<Paginated<Transaction, BlockAndTxHash>> {
    let block_number = api_try!(data.get_block_number_by_position(&block_position).await);

    data.transaction_page(block_number, query).await.into()
}

pub fn api_scope(pool: ConnectionPool, cache: BlockDetailsCache) -> Scope {
    let data = ApiBlockData::new(pool, cache);

    web::scope("block")
        .data(data)
        .route("", web::get().to(block_pagination))
        .route("{block_number}", web::get().to(block_by_number))
        .route(
            "{block_number}/transaction",
            web::get().to(block_transactions),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_server::rest::v02::{
        test_utils::{deserialize_response_result, TestServerConfig},
        SharedData,
    };
    use zksync_api_types::v02::{
        pagination::PaginationDirection, transaction::TransactionData, ApiVersion,
    };

    #[actix_rt::test]
    #[cfg_attr(
        not(feature = "api_test"),
        ignore = "Use `zk test rust-api` command to perform this test"
    )]
    async fn v02_test_block_scope() -> anyhow::Result<()> {
        let cfg = TestServerConfig::default();
        cfg.fill_database().await?;

        let shared_data = SharedData {
            net: cfg.config.chain.eth.network,
            api_version: ApiVersion::V02,
        };
        let (client, server) = cfg.start_server(
            |cfg: &TestServerConfig| api_scope(cfg.pool.clone(), BlockDetailsCache::new(10)),
            shared_data,
        );

        let query = PaginationQuery {
            from: BlockNumber(1),
            limit: 3,
            direction: PaginationDirection::Newer,
        };
        let expected_blocks: Paginated<BlockInfo, BlockNumber> = {
            let mut storage = cfg.pool.access_storage().await?;
            storage
                .paginate(&query)
                .await
                .map_err(|err| anyhow::anyhow!(err.message))?
        };

        let response = client.block_by_number_v02("2").await?;
        let block: BlockInfo = deserialize_response_result(response)?;
        assert_eq!(block, expected_blocks.list[1]);

        let response = client.block_pagination_v02(&query).await?;
        let paginated: Paginated<BlockInfo, BlockNumber> = deserialize_response_result(response)?;
        assert_eq!(paginated, expected_blocks);

        let block_number = BlockNumber(3);
        let expected_txs = {
            let mut storage = cfg.pool.access_storage().await?;
            storage
                .chain()
                .block_schema()
                .get_block_transactions(block_number)
                .await?
        };
        assert!(expected_txs.len() >= 3);
        let tx_hash_str = expected_txs.first().unwrap().tx_hash.as_str();
        let tx_hash = TxHash::from_str(tx_hash_str).unwrap();

        let query = PaginationQuery {
            from: tx_hash,
            limit: 2,
            direction: PaginationDirection::Older,
        };

        let response = client
            .block_transactions_v02(&query, &*block_number.to_string())
            .await?;
        let paginated: Paginated<Transaction, BlockAndTxHash> =
            deserialize_response_result(response)?;
        assert_eq!(paginated.pagination.count as usize, expected_txs.len());
        assert_eq!(paginated.pagination.limit, query.limit);
        assert_eq!(paginated.list.len(), query.limit as usize);
        assert_eq!(paginated.pagination.direction, PaginationDirection::Older);
        assert_eq!(paginated.pagination.from.tx_hash, tx_hash);
        assert_eq!(paginated.pagination.from.block_number, block_number);

        for (tx, expected_tx) in paginated.list.into_iter().zip(expected_txs) {
            assert_eq!(
                tx.tx_hash.to_string().replace("sync-tx:", "0x"),
                expected_tx.tx_hash
            );
            assert_eq!(tx.created_at, Some(expected_tx.created_at));
            assert_eq!(*tx.block_number.unwrap(), expected_tx.block_number as u32);
            assert_eq!(tx.fail_reason, expected_tx.fail_reason);
            if matches!(tx.op, TransactionData::L2(_)) {
                assert_eq!(serde_json::to_value(tx.op).unwrap(), expected_tx.op);
            }
        }

        server.stop().await;
        Ok(())
    }
}
