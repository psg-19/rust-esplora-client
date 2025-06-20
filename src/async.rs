// Bitcoin Dev Kit
// Written in 2020 by Alekos Filini <alekos.filini@gmail.com>
//
// Copyright (c) 2020-2021 Bitcoin Dev Kit Developers
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! Esplora by way of `reqwest` HTTP client.

use bitcoin::consensus::{deserialize, serialize, Decodable, Encodable};
use bitcoin::hashes::{sha256, Hash};
use bitcoin::hex::{DisplayHex, FromHex};
use bitcoin::Address;
use bitcoin::{
    block::Header as BlockHeader, Block, BlockHash, MerkleBlock, Script, Transaction, Txid,
};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::str::FromStr;

use crate::api::AddressStats;
use crate::{
    BlockStatus, BlockSummary, Builder, Error, MerkleProof, OutputStatus, Tx, TxStatus,
    BASE_BACKOFF_MILLIS, RETRYABLE_ERROR_CODES,
};
use async_minreq::{Method, Request};
#[allow(unused_imports)]
use log::{debug, error, info, trace};

#[derive(Debug, Clone)]
pub struct AsyncClient<S = DefaultSleeper> {
    /// The URL of the Esplora Server.
    url: String,
    /// Number of times to retry a request.
    max_retries: usize,
    /// Default headers (applied to every request).
    headers: HashMap<String, String>,
    /// Marker for the sleeper.
    marker: PhantomData<S>,
}

impl<S: Sleeper> AsyncClient<S> {
    /// Build an async client from a builder
    pub fn from_builder(builder: Builder) -> Result<Self, Error> {
        Ok(AsyncClient {
            url: builder.base_url,
            max_retries: builder.max_retries,
            headers: builder.headers,
            marker: PhantomData,
        })
    }

    pub fn from_client(url: String, headers: HashMap<String, String>) -> Self {
        AsyncClient {
            url,
            headers,
            max_retries: crate::DEFAULT_MAX_RETRIES,
            marker: PhantomData,
        }
    }
    /// Make an HTTP GET request to given URL, deserializing to any `T` that
    /// implement [`bitcoin::consensus::Decodable`].
    ///
    /// It should be used when requesting Esplora endpoints that can be directly
    /// deserialized to native `rust-bitcoin` types, which implements
    /// [`bitcoin::consensus::Decodable`] from `&[u8]`.
    ///
    /// # Errors
    ///
    /// This function will return an error either from the HTTP client, or the
    /// [`bitcoin::consensus::Decodable`] deserialization.
    async fn get_response<T: Decodable>(&self, path: &str) -> Result<T, Error> {
        let url = format!("{}{}", self.url, path);
        let response = self.get_with_retry(&url).await?;

        if response.status_code > 299 {
            return Err(Error::HttpResponse {
                status: response.status_code as u16,
                message: match response.as_str() {
                    Ok(resp) => resp.to_string(),
                    Err(_) => return Err(Error::InvalidResponse),
                },
            });
        }

        Ok(deserialize::<T>(response.as_bytes())?)
    }

    /// Make an HTTP GET request to given URL, deserializing to `Option<T>`.
    ///
    /// It uses [`AsyncEsploraClient::get_response`] internally.
    ///
    /// See [`AsyncEsploraClient::get_response`] above for full documentation.
    async fn get_opt_response<T: Decodable>(&self, path: &str) -> Result<Option<T>, Error> {
        match self.get_response::<T>(path).await {
            Ok(res) => Ok(Some(res)),
            Err(Error::HttpResponse { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Make an HTTP GET request to given URL, deserializing to any `T` that
    /// implements [`serde::de::DeserializeOwned`].
    ///
    /// It should be used when requesting Esplora endpoints that have a specific
    /// defined API, mostly defined in [`crate::api`].
    ///
    /// # Errors
    ///
    /// This function will return an error either from the HTTP client, or the
    /// [`serde::de::DeserializeOwned`] deserialization.
    async fn get_response_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, Error> {
        let url = format!("{}{}", self.url, path);
        let response = self.get_with_retry(&url).await?;

        if response.status_code > 299 {
            return Err(Error::HttpResponse {
                status: response.status_code as u16,
                message: match response.as_str() {
                    Ok(resp) => resp.to_string(),
                    Err(_) => return Err(Error::InvalidResponse),
                },
            });
        }
        serde_json::from_str(match response.as_str() {
            Ok(resp) => resp,
            Err(_) => return Err(Error::InvalidResponse),
        })
        .map_err(Error::Json)
    }

    /// Make an HTTP GET request to given URL, deserializing to `Option<T>`.
    ///
    /// It uses [`AsyncEsploraClient::get_response_json`] internally.
    ///
    /// See [`AsyncEsploraClient::get_response_json`] above for full
    /// documentation.
    async fn get_opt_response_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
    ) -> Result<Option<T>, Error> {
        match self.get_response_json(url).await {
            Ok(res) => Ok(Some(res)),
            Err(Error::HttpResponse { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Make an HTTP GET request to given URL, deserializing to any `T` that
    /// implements [`bitcoin::consensus::Decodable`].
    ///
    /// It should be used when requesting Esplora endpoints that are expected
    /// to return a hex string decodable to native `rust-bitcoin` types which
    /// implement [`bitcoin::consensus::Decodable`] from `&[u8]`.
    ///
    /// # Errors
    ///
    /// This function will return an error either from the HTTP client, or the
    /// [`bitcoin::consensus::Decodable`] deserialization.
    async fn get_response_hex<T: Decodable>(&self, path: &str) -> Result<T, Error> {
        let url = format!("{}{}", self.url, path);
        let response = self.get_with_retry(&url).await?;

        if response.status_code > 299 {
            return Err(Error::HttpResponse {
                status: response.status_code as u16,
                message: match response.as_str() {
                    Ok(resp) => resp.to_string(),
                    Err(_) => return Err(Error::InvalidResponse),
                },
            });
        }
        let hex_str = match response.as_str() {
            Ok(resp) => resp.to_string(),
            Err(_) => return Err(Error::InvalidResponse),
        };
        Ok(deserialize(&Vec::from_hex(&hex_str)?)?)
    }

    /// Make an HTTP GET request to given URL, deserializing to `Option<T>`.
    ///
    /// It uses [`AsyncEsploraClient::get_response_hex`] internally.
    ///
    /// See [`AsyncEsploraClient::get_response_hex`] above for full
    /// documentation.
    async fn get_opt_response_hex<T: Decodable>(&self, path: &str) -> Result<Option<T>, Error> {
        match self.get_response_hex(path).await {
            Ok(res) => Ok(Some(res)),
            Err(Error::HttpResponse { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Make an HTTP GET request to given URL, deserializing to `String`.
    ///
    /// It should be used when requesting Esplora endpoints that can return
    /// `String` formatted data that can be parsed downstream.
    ///
    /// # Errors
    ///
    /// This function will return an error either from the HTTP client.
    async fn get_response_text(&self, path: &str) -> Result<String, Error> {
        let url = format!("{}{}", self.url, path);
        let response = self.get_with_retry(&url).await?;

        if response.status_code > 299 {
            return Err(Error::HttpResponse {
                status: response.status_code as u16,
                message: match response.as_str() {
                    Ok(resp) => resp.to_string(),
                    Err(_) => return Err(Error::InvalidResponse),
                },
            });
        }
        Ok(match response.as_str() {
            Ok(resp) => resp.to_string(),
            Err(_) => return Err(Error::InvalidResponse),
        })
    }

    /// Make an HTTP GET request to given URL, deserializing to `Option<T>`.
    ///
    /// It uses [`AsyncEsploraClient::get_response_text`] internally.
    ///
    /// See [`AsyncEsploraClient::get_response_text`] above for full
    /// documentation.
    async fn get_opt_response_text(&self, path: &str) -> Result<Option<String>, Error> {
        match self.get_response_text(path).await {
            Ok(s) => Ok(Some(s)),
            Err(Error::HttpResponse { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Make an HTTP POST request to given URL, serializing from any `T` that
    /// implement [`bitcoin::consensus::Encodable`].
    ///
    /// It should be used when requesting Esplora endpoints that expected a
    /// native bitcoin type serialized with [`bitcoin::consensus::Encodable`].
    ///
    /// # Errors
    ///
    /// This function will return an error either from the HTTP client, or the
    /// [`bitcoin::consensus::Encodable`] serialization.
    async fn post_request_hex<T: Encodable>(&self, path: &str, body: T) -> Result<(), Error> {
        let url = format!("{}{}", self.url, path);
        let body = serialize::<T>(&body).to_lower_hex_string();

        let mut request = Request::new(Method::Post, &url).with_body(body);
        for (key, value) in &self.headers {
            request = request.with_header(key, value);
        }

        let response = request.send().await.map_err(Error::AsyncMinreq)?;
        if response.status_code > 299 {
            return Err(Error::HttpResponse {
                status: response.status_code as u16,
                message: match response.as_str() {
                    Ok(resp) => resp.to_string(),
                    Err(_) => return Err(Error::InvalidResponse),
                },
            });
        }
        Ok(())
    }

    /// Get a [`Transaction`] option given its [`Txid`]
    pub async fn get_tx(&self, txid: &Txid) -> Result<Option<Transaction>, Error> {
        self.get_opt_response(&format!("/tx/{txid}/raw")).await
    }

    /// Get a [`Transaction`] given its [`Txid`].
    pub async fn get_tx_no_opt(&self, txid: &Txid) -> Result<Transaction, Error> {
        match self.get_tx(txid).await {
            Ok(Some(tx)) => Ok(tx),
            Ok(None) => Err(Error::TransactionNotFound(*txid)), //look into
            Err(e) => Err(e),
        }
    }

    /// Get a [`Txid`] of a transaction given its index in a block with a given
    /// hash.
    pub async fn get_txid_at_block_index(
        &self,
        block_hash: &BlockHash,
        index: usize,
    ) -> Result<Option<Txid>, Error> {
        match self
            .get_opt_response_text(&format!("/block/{block_hash}/txid/{index}"))
            .await?
        {
            Some(s) => Ok(Some(Txid::from_str(&s).map_err(Error::HexToArray)?)),
            None => Ok(None),
        }
    }

    /// Get the status of a [`Transaction`] given its [`Txid`].
    pub async fn get_tx_status(&self, txid: &Txid) -> Result<TxStatus, Error> {
        self.get_response_json(&format!("/tx/{txid}/status")).await
    }

    /// Get transaction info given it's [`Txid`].
    pub async fn get_tx_info(&self, txid: &Txid) -> Result<Option<Tx>, Error> {
        self.get_opt_response_json(&format!("/tx/{txid}")).await
    }

    /// Get a [`BlockHeader`] given a particular block hash.
    pub async fn get_header_by_hash(&self, block_hash: &BlockHash) -> Result<BlockHeader, Error> {
        self.get_response_hex(&format!("/block/{block_hash}/header"))
            .await
    }

    /// Get the [`BlockStatus`] given a particular [`BlockHash`].
    pub async fn get_block_status(&self, block_hash: &BlockHash) -> Result<BlockStatus, Error> {
        self.get_response_json(&format!("/block/{block_hash}/status"))
            .await
    }

    /// Get a [`Block`] given a particular [`BlockHash`].
    pub async fn get_block_by_hash(&self, block_hash: &BlockHash) -> Result<Option<Block>, Error> {
        self.get_opt_response(&format!("/block/{block_hash}/raw"))
            .await
    }

    /// Get a merkle inclusion proof for a [`Transaction`] with the given
    /// [`Txid`].
    pub async fn get_merkle_proof(&self, tx_hash: &Txid) -> Result<Option<MerkleProof>, Error> {
        self.get_opt_response_json(&format!("/tx/{tx_hash}/merkle-proof"))
            .await
    }

    /// Get a [`MerkleBlock`] inclusion proof for a [`Transaction`] with the
    /// given [`Txid`].
    pub async fn get_merkle_block(&self, tx_hash: &Txid) -> Result<Option<MerkleBlock>, Error> {
        self.get_opt_response_hex(&format!("/tx/{tx_hash}/merkleblock-proof"))
            .await
    }

    /// Get the spending status of an output given a [`Txid`] and the output
    /// index.
    pub async fn get_output_status(
        &self,
        txid: &Txid,
        index: u64,
    ) -> Result<Option<OutputStatus>, Error> {
        self.get_opt_response_json(&format!("/tx/{txid}/outspend/{index}"))
            .await
    }

    /// Broadcast a [`Transaction`] to Esplora
    pub async fn broadcast(&self, transaction: &Transaction) -> Result<(), Error> {
        self.post_request_hex("/tx", transaction).await
    }

    /// Get the current height of the blockchain tip
    pub async fn get_height(&self) -> Result<u32, Error> {
        self.get_response_text("/blocks/tip/height")
            .await
            .map(|height| u32::from_str(&height).map_err(Error::Parsing))?
    }

    /// Get the [`BlockHash`] of the current blockchain tip.
    pub async fn get_tip_hash(&self) -> Result<BlockHash, Error> {
        self.get_response_text("/blocks/tip/hash")
            .await
            .map(|block_hash| BlockHash::from_str(&block_hash).map_err(Error::HexToArray))?
    }

    /// Get the [`BlockHash`] of a specific block height
    pub async fn get_block_hash(&self, block_height: u32) -> Result<BlockHash, Error> {
        self.get_response_text(&format!("/block-height/{block_height}"))
            .await
            .map(|block_hash| BlockHash::from_str(&block_hash).map_err(Error::HexToArray))?
    }

    /// Get information about a specific address, includes confirmed balance and transactions in
    /// the mempool.
    pub async fn get_address_stats(&self, address: &Address) -> Result<AddressStats, Error> {
        let path = format!("/address/{address}");
        self.get_response_json(&path).await
    }

    /// Get transaction history for the specified address/scripthash, sorted with newest first.
    ///
    /// Returns up to 50 mempool transactions plus the first 25 confirmed transactions.
    /// More can be requested by specifying the last txid seen by the previous query.
    pub async fn get_address_txs(
        &self,
        address: &Address,
        last_seen: Option<Txid>,
    ) -> Result<Vec<Tx>, Error> {
        let path = match last_seen {
            Some(last_seen) => format!("/address/{address}/txs/chain/{last_seen}"),
            None => format!("/address/{address}/txs"),
        };

        self.get_response_json(&path).await
    }

    /// Get confirmed transaction history for the specified address/scripthash,
    /// sorted with newest first. Returns 25 transactions per page.
    /// More can be requested by specifying the last txid seen by the previous
    /// query.
    pub async fn scripthash_txs(
        &self,
        script: &Script,
        last_seen: Option<Txid>,
    ) -> Result<Vec<Tx>, Error> {
        let script_hash = sha256::Hash::hash(script.as_bytes());
        let path = match last_seen {
            Some(last_seen) => format!("/scripthash/{:x}/txs/chain/{}", script_hash, last_seen),
            None => format!("/scripthash/{:x}/txs", script_hash),
        };

        self.get_response_json(&path).await
    }

    /// Get an map where the key is the confirmation target (in number of
    /// blocks) and the value is the estimated feerate (in sat/vB).
    pub async fn get_fee_estimates(&self) -> Result<HashMap<u16, f64>, Error> {
        self.get_response_json("/fee-estimates").await
    }

    /// Gets some recent block summaries starting at the tip or at `height` if
    /// provided.
    ///
    /// The maximum number of summaries returned depends on the backend itself:
    /// esplora returns `10` while [mempool.space](https://mempool.space/docs/api) returns `15`.
    pub async fn get_blocks(&self, height: Option<u32>) -> Result<Vec<BlockSummary>, Error> {
        let path = match height {
            Some(height) => format!("/blocks/{height}"),
            None => "/blocks".to_string(),
        };
        let blocks: Vec<BlockSummary> = self.get_response_json(&path).await?;
        if blocks.is_empty() {
            return Err(Error::InvalidResponse);
        }
        Ok(blocks)
    }

    /// Get the underlying base URL.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Sends a GET request to the given `url`, retrying failed attempts
    /// for retryable error codes until max retries hit.
    async fn get_with_retry(&self, url: &str) -> Result<async_minreq::Response, Error> {
        let mut delay = BASE_BACKOFF_MILLIS;
        let mut attempts = 0;

        loop {
            let mut request = Request::new(Method::Get, url);
            for (key, value) in &self.headers {
                request = request.with_header(key, value);
            }

            match request.send().await? {
                resp if attempts < self.max_retries && is_status_retryable(resp.status_code) => {
                    S::sleep(delay).await;
                    attempts += 1;
                    delay *= 2;
                }
                resp => return Ok(resp),
            }
        }
    }
}

fn is_status_retryable(status: i32) -> bool {
    RETRYABLE_ERROR_CODES.contains(&(status as u16))
}

pub trait Sleeper: 'static {
    type Sleep: std::future::Future<Output = ()>;
    fn sleep(dur: std::time::Duration) -> Self::Sleep;
}

#[derive(Debug, Clone, Copy)]
pub struct DefaultSleeper;

#[cfg(any(test, feature = "tokio"))]
impl Sleeper for DefaultSleeper {
    type Sleep = tokio::time::Sleep;

    fn sleep(dur: std::time::Duration) -> Self::Sleep {
        tokio::time::sleep(dur)
    }
}
