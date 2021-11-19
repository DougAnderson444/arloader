//! SDK for uploading files in bulk to [Arweave](https://www.arweave.org/).
//!
//! Files can't just be uploaded in a post it and forget manner to Arweave since their data needs to be
//! written to the blockchain by node operators and that doesn't happen instantaneously. This SDK aims to
//! make the process of uploading large numbers of files as seamless as possible. In addition to providing
//! highly performant, streaming uploads, it also includes status logging and reporting features through which
//! complete upload processes can be developed, including uploading files, updating statuses and re-uploading
//! files from filtered sets of statuses.

#![feature(derive_default_enum)]
use crate::solana::{create_sol_transaction, get_sol_ar_signature, SigResponse, FLOOR, RATE};
use blake3;
use chrono::Utc;
use futures::{
    future::{try_join, try_join_all},
    stream, Stream, StreamExt,
};
use infer;
use log::debug;
use num_bigint::BigUint;
use reqwest::{
    self,
    header::{ACCEPT, CONTENT_TYPE},
    StatusCode as ResponseStatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use solana_sdk::signer::keypair::Keypair;
use std::{collections::HashMap, fmt::Write, path::PathBuf, str::FromStr};
use tokio::fs;
use url::Url;

pub mod bundle;
pub mod crypto;
pub mod error;
pub mod merkle;
pub mod solana;
pub mod status;
pub mod transaction;
pub mod utils;

use bundle::DataItem;
use error::Error;
use merkle::{generate_data_root, generate_leaves, resolve_proofs};
use status::{Status, StatusCode};
use transaction::{Base64, FromUtf8Strs, Tag, ToItems, Transaction};

const VERSION: &'static str = env!("CARGO_PKG_VERSION");

/// Winstons are a sub unit of the native Arweave network token, AR. There are 10<sup>12</sup> Winstons per AR.
pub const WINSTONS_PER_AR: u64 = 1000000000000;
pub const BLOCK_SIZE: u64 = 1024 * 256;

#[derive(Serialize, Deserialize, Debug)]
struct OraclePrice {
    pub arweave: OraclePricePair,
    pub solana: OraclePricePair,
}

#[derive(Serialize, Deserialize, Debug)]
struct OraclePricePair {
    pub usd: f32,
}

/// Uploads files matching glob pattern, returning a stream of [`Status`] structs.
pub fn upload_files_stream<'a, IP>(
    arweave: &'a Arweave,
    paths_iter: IP,
    log_dir: Option<PathBuf>,
    last_tx: Option<Base64>,
    price_terms: (u64, u64),
    buffer: usize,
) -> impl Stream<Item = Result<Status, Error>> + 'a
where
    IP: Iterator<Item = PathBuf> + Send + Sync + 'a,
{
    stream::iter(paths_iter)
        .map(move |p| {
            arweave.upload_file_from_path(p, log_dir.clone(), None, last_tx.clone(), price_terms)
        })
        .buffer_unordered(buffer)
}

/// Uploads files matching glob pattern, returning a stream of [`Status`] structs, paying with SOL.
pub fn upload_files_with_sol_stream<'a, IP>(
    arweave: &'a Arweave,
    paths_iter: IP,
    log_dir: Option<PathBuf>,
    last_tx: Option<Base64>,
    price_terms: (u64, u64),
    solana_url: Url,
    sol_ar_url: Url,
    from_keypair: &'a Keypair,
    buffer: usize,
) -> impl Stream<Item = Result<Status, Error>> + 'a
where
    IP: Iterator<Item = PathBuf> + Send + Sync + 'a,
{
    stream::iter(paths_iter)
        .map(move |p| {
            arweave.upload_file_from_path_with_sol(
                p,
                log_dir.clone(),
                None,
                last_tx.clone(),
                price_terms,
                solana_url.clone(),
                sol_ar_url.clone(),
                from_keypair,
            )
        })
        .buffer_unordered(buffer)
}

/// Queries network and updates locally stored [`Status`] structs.
pub fn update_statuses_stream<'a, IP>(
    arweave: &'a Arweave,
    paths_iter: IP,
    log_dir: PathBuf,
    buffer: usize,
) -> impl Stream<Item = Result<Status, Error>> + 'a
where
    IP: Iterator<Item = PathBuf> + Send + Sync + 'a,
{
    stream::iter(paths_iter)
        .map(move |p| arweave.update_status(p, log_dir.clone()))
        .buffer_unordered(buffer)
}

/// Struct on which [`Methods`] for interacting with the network are implemented.
pub struct Arweave {
    pub name: String,
    pub units: String,
    pub base_url: Url,
    pub crypto: crypto::Provider,
}

impl Arweave {
    pub async fn from_keypair_path(
        keypair_path: PathBuf,
        base_url: Option<Url>,
    ) -> Result<Arweave, Error> {
        Ok(Arweave {
            name: String::from("arweave"),
            units: String::from("winstons"),
            base_url: base_url.unwrap_or(Url::from_str("https://arweave.net/")?),
            crypto: crypto::Provider::from_keypair_path(keypair_path).await?,
        })
    }

    /// Returns the balance of the wallet.
    pub async fn get_wallet_balance(
        &self,
        wallet_address: Option<String>,
    ) -> Result<BigUint, Error> {
        let wallet_address = if let Some(wallet_address) = wallet_address {
            wallet_address
        } else {
            self.crypto.wallet_address()?.to_string()
        };
        let url = self
            .base_url
            .join(&format!("wallet/{}/balance", &wallet_address))?;
        let winstons = reqwest::get(url).await?.json::<u64>().await?;
        Ok(BigUint::from(winstons))
    }

    /// Returns price of uploading data to the network in winstons and USD per AR and USD per SOL
    /// as a BigUint with two decimals.
    pub async fn get_price(&self, bytes: &u64) -> Result<(BigUint, BigUint, BigUint), Error> {
        let url = self.base_url.join("price/")?.join(&bytes.to_string())?;
        let winstons_per_bytes = reqwest::get(url).await?.json::<u64>().await?;
        let winstons_per_bytes = BigUint::from(winstons_per_bytes);
        let oracle_url =
            "https://api.coingecko.com/api/v3/simple/price?ids=arweave,solana&vs_currencies=usd";

        let resp = reqwest::get(oracle_url).await?;

        let prices = resp.json::<OraclePrice>().await?;

        let usd_per_ar: BigUint = BigUint::from((prices.arweave.usd * 100.0).floor() as u32);
        let usd_per_sol: BigUint = BigUint::from((prices.solana.usd * 100.0).floor() as u32);

        Ok((winstons_per_bytes, usd_per_ar, usd_per_sol))
    }

    pub async fn get_price_terms(&self, reward_mult: f32) -> Result<(u64, u64), Error> {
        let (prices1, prices2) = try_join(
            self.get_price(&(256 * 1024)),
            self.get_price(&(256 * 1024 * 2)),
        )
        .await?;
        let base = (prices1.0.to_u64_digits()[0] as f32 * reward_mult) as u64;
        let incremental = (prices2.0.to_u64_digits()[0] as f32 * reward_mult) as u64 - &base;
        Ok((base, incremental))
    }

    pub async fn get_transaction(&self, id: &Base64) -> Result<Transaction, Error> {
        let url = self.base_url.join("tx/")?.join(&id.to_string())?;
        let resp = reqwest::get(url).await?.json::<Transaction>().await?;
        Ok(resp)
    }

    pub async fn create_transaction(
        &self,
        data: Vec<u8>,
        other_tags: Option<Vec<Tag<Base64>>>,
        last_tx: Option<Base64>,
        price_terms: (u64, u64),
    ) -> Result<Transaction, Error> {
        let chunks = generate_leaves(data.clone(), &self.crypto)?;
        let root = generate_data_root(chunks.clone(), &self.crypto)?;
        let data_root = Base64(root.id.clone().into_iter().collect());
        let proofs = resolve_proofs(root, None)?;
        let owner = self.crypto.keypair_modulus()?;

        // Get content type from [magic numbers](https://developer.mozilla.org/en-US/docs/Web/HTTP/Basics_of_HTTP/MIME_types)
        // and include additional tags if any.
        let content_type = if let Some(kind) = infer::get(&data) {
            kind.mime_type()
        } else {
            "application/octet-stream"
        };
        let mut tags = vec![
            Tag::<Base64>::from_utf8_strs("Content-Type", content_type)?,
            Tag::<Base64>::from_utf8_strs("User-Agent", &format!("arloader/{}", VERSION))?,
        ];

        // Add other tags if provided.
        if let Some(other_tags) = other_tags {
            tags.extend(other_tags);
        }

        // Fetch and set last_tx if not provided (primarily for testing).
        let last_tx = if let Some(last_tx) = last_tx {
            last_tx
        } else {
            let resp = reqwest::get(self.base_url.join("tx_anchor")?).await?;
            debug!("last_tx: {}", resp.status());
            let last_tx_str = resp.text().await?;
            Base64::from_str(&last_tx_str)?
        };

        let data_len = data.len() as u64;
        let blocks_len = data_len / BLOCK_SIZE + (data_len % BLOCK_SIZE != 0) as u64;
        let reward = price_terms.0 + price_terms.1 * (blocks_len - 1);

        Ok(Transaction {
            format: 2,
            data_size: data_len.clone(),
            data: Base64(data),
            data_root,
            tags,
            reward,
            owner,
            last_tx,
            chunks,
            proofs,
            ..Default::default()
        })
    }

    pub async fn create_transaction_from_file_path(
        &self,
        file_path: PathBuf,
        other_tags: Option<Vec<Tag<Base64>>>,
        last_tx: Option<Base64>,
        price_terms: (u64, u64),
    ) -> Result<Transaction, Error> {
        let data = fs::read(file_path).await?;
        self.create_transaction(data, other_tags, last_tx, price_terms)
            .await
    }

    /// Gets deep hash, signs and sets signature and id.
    pub fn sign_transaction(&self, mut transaction: Transaction) -> Result<Transaction, Error> {
        let deep_hash_item = transaction.to_deep_hash_item()?;
        let deep_hash = self.crypto.deep_hash(deep_hash_item)?;
        let signature = self.crypto.sign(&deep_hash)?;
        let id = self.crypto.hash_sha256(&signature)?;
        transaction.signature = Base64(signature);
        transaction.id = Base64(id.to_vec());
        Ok(transaction)
    }

    pub async fn post_transaction(
        &self,
        signed_transaction: &Transaction,
        file_path: Option<PathBuf>,
    ) -> Result<Status, Error> {
        if signed_transaction.id.0.is_empty() {
            return Err(error::Error::UnsignedTransaction.into());
        }

        let url = self.base_url.join("tx/")?;
        let client = reqwest::Client::new();
        let resp = client
            .post(url)
            .json(&signed_transaction)
            .header(&ACCEPT, "application/json")
            .header(&CONTENT_TYPE, "application/json")
            .send()
            .await?;
        debug!("post_transaction {:?}", &resp);
        assert_eq!(resp.status().as_u16(), 200);

        let status = Status {
            id: signed_transaction.id.clone(),
            reward: signed_transaction.reward,
            file_path,
            ..Default::default()
        };

        Ok(status)
    }

    pub async fn get_pending_count(&self) -> Result<usize, Error> {
        let url = self.base_url.join("tx/pending")?;
        let tx_ids: Vec<String> = reqwest::get(url).await?.json().await?;
        Ok(tx_ids.len())
    }

    pub async fn get_raw_status(&self, id: &Base64) -> Result<reqwest::Response, Error> {
        let url = self.base_url.join(&format!("tx/{}/status", id))?;
        let resp = reqwest::get(url).await?;
        Ok(resp)
    }

    /// Writes Status Json to `log_dir` with file name based on BLAKE3 hash of `status.file_path`.
    ///
    /// This is done to facilitate checking the status of uploaded file and also means that only
    /// one status object can exist for a given `file_path`. If for some reason you wanted to record
    /// statuses for multiple uploads of the same file you can provide a different `log_dir` (or copy the
    /// file to a different directory and upload from there).
    pub async fn write_status(&self, status: Status, log_dir: PathBuf) -> Result<(), Error> {
        let file_stem = if let Some(file_path) = &status.file_path {
            if status.id.0.is_empty() {
                return Err(error::Error::UnsignedTransaction.into());
            }
            blake3::hash(file_path.to_str().unwrap().as_bytes()).to_string()
        } else {
            format!("id_{}", status.id)
        };
        fs::write(
            log_dir.join(file_stem).with_extension("json"),
            serde_json::to_string(&status)?,
        )
        .await?;
        Ok(())
    }

    pub async fn read_status(&self, file_path: PathBuf, log_dir: PathBuf) -> Result<Status, Error> {
        let file_path_hash = blake3::hash(file_path.to_str().unwrap().as_bytes());

        let status_path = log_dir
            .join(file_path_hash.to_string())
            .with_extension("json");

        if status_path.exists() {
            let data = fs::read_to_string(status_path).await?;
            let status: Status = serde_json::from_str(&data)?;
            Ok(status)
        } else {
            Err(Error::StatusNotFound)
        }
    }

    pub async fn read_statuses<IP>(
        &self,
        paths_iter: IP,
        log_dir: PathBuf,
    ) -> Result<Vec<Status>, Error>
    where
        IP: Iterator<Item = PathBuf> + Send,
    {
        try_join_all(paths_iter.map(|p| self.read_status(p, log_dir.clone()))).await
    }

    pub async fn status_summary<IP>(
        &self,
        paths_iter: IP,
        log_dir: PathBuf,
    ) -> Result<String, Error>
    where
        IP: Iterator<Item = PathBuf> + Send,
    {
        let statuses = self.read_statuses(paths_iter, log_dir).await?;
        let status_counts: HashMap<StatusCode, u32> =
            statuses
                .into_iter()
                .fold(HashMap::new(), |mut map, status| {
                    *map.entry(status.status).or_insert(0) += 1;
                    map
                });

        let mut total = 0;
        let mut output = String::new();
        writeln!(output, " {:<15}  {:>10}", "status", "count")?;
        writeln!(output, "{:-<29}", "")?;
        for k in vec![
            StatusCode::Submitted,
            StatusCode::Pending,
            StatusCode::NotFound,
            StatusCode::Confirmed,
        ] {
            let v = status_counts.get(&k).unwrap_or(&0);
            writeln!(output, " {:<16} {:>10}", &k.to_string(), v)?;
            total += v;
        }

        writeln!(output, "{:-<29}", "")?;
        writeln!(output, " {:<15}  {:>10}", "Total", total)?;

        Ok(output)
    }

    pub async fn write_manifest<IP>(&self, paths_iter: IP, log_dir: PathBuf) -> Result<(), Error>
    where
        IP: Iterator<Item = PathBuf> + Send,
    {
        let statuses: Vec<Value> = self
            .read_statuses(paths_iter, log_dir.clone())
            .await?
            .into_iter()
            .map(|s| json!({s.file_path.unwrap().display().to_string(): {"id": s.id.to_string()}}))
            .collect();

        let manifest = json!({
            "manifest": "arweave/paths",
            "version": "0.1.0",
            "paths": statuses
        });

        fs::write(
            log_dir.join("manifest").with_extension("json"),
            serde_json::to_string(&manifest)?
                .replace("[", "")
                .replace("]", ""),
        )
        .await?;

        Ok(())
    }

    pub async fn update_status(
        &self,
        file_path: PathBuf,
        log_dir: PathBuf,
    ) -> Result<Status, Error> {
        let mut status = self.read_status(file_path, log_dir.clone()).await?;
        let resp = self.get_raw_status(&status.id).await?;
        status.last_modified = Utc::now();
        match resp.status() {
            ResponseStatusCode::OK => {
                let resp_string = resp.text().await?;
                if &resp_string == &String::from("Pending") {
                    status.status = StatusCode::Pending;
                } else {
                    status.raw_status = Some(serde_json::from_str(&resp_string)?);
                    status.status = StatusCode::Confirmed;
                }
            }
            ResponseStatusCode::ACCEPTED => {
                status.status = StatusCode::Pending;
            }
            ResponseStatusCode::NOT_FOUND => {
                status.status = StatusCode::NotFound;
            }
            _ => unreachable!(),
        }
        self.write_status(status.clone(), log_dir).await?;
        Ok(status)
    }

    pub async fn update_statuses<IP>(
        &self,
        paths_iter: IP,
        log_dir: PathBuf,
    ) -> Result<Vec<Status>, Error>
    where
        IP: Iterator<Item = PathBuf> + Send,
    {
        try_join_all(paths_iter.map(|p| self.update_status(p, log_dir.clone()))).await
    }

    pub async fn upload_file_from_path(
        &self,
        file_path: PathBuf,
        log_dir: Option<PathBuf>,
        additional_tags: Option<Vec<Tag<Base64>>>,
        last_tx: Option<Base64>,
        price_terms: (u64, u64),
    ) -> Result<Status, Error> {
        let transaction = self
            .create_transaction_from_file_path(
                file_path.clone(),
                additional_tags,
                last_tx,
                price_terms,
            )
            .await?;
        let signed_transaction = self.sign_transaction(transaction)?;
        let status = self
            .post_transaction(&signed_transaction, Some(file_path))
            .await?;

        if let Some(log_dir) = log_dir {
            self.write_status(status.clone(), log_dir).await?;
        }
        Ok(status)
    }

    /// Signs transaction with sol_ar service.
    pub async fn sign_transaction_with_sol(
        &self,
        mut transaction: Transaction,
        solana_url: Url,
        sol_ar_url: Url,
        from_keypair: &Keypair,
    ) -> Result<(Transaction, SigResponse), Error> {
        let lamports = std::cmp::max(&transaction.reward / RATE, FLOOR);

        let sol_tx = create_sol_transaction(solana_url, from_keypair, lamports).await?;
        let sig_response =
            get_sol_ar_signature(sol_ar_url, transaction.to_deep_hash_item()?, sol_tx).await?;
        let sig_response_copy = sig_response.clone();
        transaction.signature = sig_response.ar_tx_sig;
        transaction.id = sig_response.ar_tx_id;
        transaction.owner = sig_response.ar_tx_owner;
        Ok((transaction, sig_response_copy))
    }

    pub async fn upload_file_from_path_with_sol(
        &self,
        file_path: PathBuf,
        log_dir: Option<PathBuf>,
        additional_tags: Option<Vec<Tag<Base64>>>,
        last_tx: Option<Base64>,
        price_terms: (u64, u64),
        solana_url: Url,
        sol_ar_url: Url,
        from_keypair: &Keypair,
    ) -> Result<Status, Error> {
        let transaction = self
            .create_transaction_from_file_path(
                file_path.clone(),
                additional_tags,
                last_tx,
                price_terms,
            )
            .await?;

        let (signed_transaction, sig_response): (Transaction, SigResponse) = self
            .sign_transaction_with_sol(transaction, solana_url, sol_ar_url, from_keypair)
            .await?;

        let mut status = self
            .post_transaction(&signed_transaction, Some(file_path))
            .await?;

        if let Some(log_dir) = log_dir {
            status.sol_sig = Some(sig_response);
            self.write_status(status.clone(), log_dir).await?;
        }
        Ok(status)
    }

    /// Uploads files from an iterator of paths.
    ///
    /// Optionally logs Status objects to `log_dir`, if provided and optionally adds tags to each
    ///  transaction from an iterator of tags that must be the same size as the paths iterator.
    pub async fn upload_files_from_paths<IP, IT>(
        &self,
        paths_iter: IP,
        log_dir: Option<PathBuf>,
        tags_iter: Option<IT>,
        last_tx: Option<Base64>,
        price_terms: (u64, u64),
    ) -> Result<Vec<Status>, Error>
    where
        IP: Iterator<Item = PathBuf> + Send,
        IT: Iterator<Item = Option<Vec<Tag<Base64>>>> + Send,
    {
        let statuses = if let Some(tags_iter) = tags_iter {
            try_join_all(paths_iter.zip(tags_iter).map(|(p, t)| {
                self.upload_file_from_path(p, log_dir.clone(), t, last_tx.clone(), price_terms)
            }))
        } else {
            try_join_all(paths_iter.map(|p| {
                self.upload_file_from_path(p, log_dir.clone(), None, last_tx.clone(), price_terms)
            }))
        }
        .await?;
        Ok(statuses)
    }

    /// Filters saved Status objects by status and/or number of confirmations. Return
    /// all statuses if no status codes or maximum confirmations are provided.
    ///
    /// If there is no raw status object and max_confirms is passed, it
    /// assumes there are zero confirms. This is designed to be used to
    /// determine whether all files have a confirmed status and to collect the
    /// paths of the files that need to be re-uploaded.
    pub async fn filter_statuses<IP>(
        &self,
        paths_iter: IP,
        log_dir: PathBuf,
        statuses: Option<Vec<StatusCode>>,
        max_confirms: Option<u64>,
    ) -> Result<Vec<Status>, Error>
    where
        IP: Iterator<Item = PathBuf> + Send,
    {
        let all_statuses = self.read_statuses(paths_iter, log_dir).await?;

        let filtered = if let Some(statuses) = statuses {
            if let Some(max_confirms) = max_confirms {
                all_statuses
                    .into_iter()
                    .filter(|s| {
                        let confirms = if let Some(raw_status) = &s.raw_status {
                            raw_status.number_of_confirmations
                        } else {
                            0
                        };
                        (&statuses.iter().any(|c| c == &s.status)) & (confirms <= max_confirms)
                    })
                    .collect()
            } else {
                all_statuses
                    .into_iter()
                    .filter(|s| statuses.iter().any(|c| c == &s.status))
                    .collect()
            }
        } else {
            if let Some(max_confirms) = max_confirms {
                all_statuses
                    .into_iter()
                    .filter(|s| {
                        let confirms = if let Some(raw_status) = &s.raw_status {
                            raw_status.number_of_confirmations
                        } else {
                            0
                        };
                        confirms <= max_confirms
                    })
                    .collect()
            } else {
                all_statuses
            }
        };

        Ok(filtered)
    }

    // Create [`data_item::DataItem`] for bundle.
    pub fn create_data_item(
        &self,
        data: Vec<u8>,
        mut tags: Vec<Tag<String>>,
    ) -> Result<DataItem, Error> {
        let content_type = if let Some(kind) = infer::get(&data) {
            kind.mime_type()
        } else {
            "application/octet-stream"
        };
        tags.extend(vec![
            Tag::<String>::from_utf8_strs("Content-Type", content_type)?,
            Tag::<String>::from_utf8_strs("User-Agent", &format!("arloader/{}", VERSION))?,
        ]);

        // let mut anchor = Base64(Vec::with_capacity(32));
        // self.crypto.fill_rand(&mut anchor.0)?;

        Ok(DataItem {
            data: Base64(data),
            tags,
            // anchor,
            ..DataItem::default()
        })
    }

    pub fn sign_data_item(&self, mut data_item: DataItem) -> Result<DataItem, Error> {
        data_item.owner = self.crypto.keypair_modulus()?;
        let deep_hash_item = data_item.to_deep_hash_item()?;
        let deep_hash = self.crypto.deep_hash(deep_hash_item)?;
        let signature = self.crypto.sign(&deep_hash)?;
        let id = self.crypto.hash_sha256(&signature)?;

        data_item.signature = Base64(signature);
        data_item.id = Base64(id.to_vec());
        Ok(data_item)
    }

    pub async fn create_data_item_from_file_path(
        &self,
        file_path: PathBuf,
        tags: Vec<Tag<String>>,
        log_dir: Option<PathBuf>,
    ) -> Result<DataItem, Error> {
        let data = fs::read(&file_path).await?;
        let data_item = self.create_data_item(data, tags)?;
        let data_item = self.sign_data_item(data_item)?;

        if let Some(log_dir) = log_dir {
            let status = Status {
                id: data_item.id.clone(),
                file_path: Some(file_path),
                ..Status::default()
            };
            self.write_status(status, log_dir).await?;
        }

        Ok(data_item)
    }

    pub async fn create_data_items_from_file_paths<IP>(
        &self,
        paths_iter: IP,
        tags: Vec<Tag<String>>,
        log_dir: Option<PathBuf>,
    ) -> Result<Vec<DataItem>, Error>
    where
        IP: Iterator<Item = PathBuf> + Send,
    {
        try_join_all(
            paths_iter
                .map(|p| self.create_data_item_from_file_path(p, tags.clone(), log_dir.clone())),
        )
        .await
    }

    pub fn create_bundle_from_data_items(
        &self,
        data_items: Vec<DataItem>,
    ) -> Result<Vec<u8>, Error> {
        let data_items_len = data_items.len() as u64;
        let (headers, binaries): (Vec<Vec<u8>>, Vec<Vec<u8>>) = data_items
            .into_iter()
            .map(|b| b.to_bundle_item().unwrap())
            .unzip();

        let binary: Vec<u8> = data_items_len
            .to_le_bytes()
            .into_iter()
            .chain([0u8; 24].into_iter())
            .chain(headers.into_iter().flatten())
            .chain(binaries.into_iter().flatten())
            .collect();

        Ok(binary)
    }

    // Tested here instead of data_item to verify signature as well.
    pub fn deserialize_bundle(&self, bundle: Vec<u8>) -> Result<Vec<DataItem>, Error> {
        let mut bundle_iter = bundle.into_iter();
        let result = [(); 8].map(|_| bundle_iter.next().unwrap());
        let number_of_data_items = u64::from_le_bytes(result) as usize;
        (0..24).for_each(|_| {
            bundle_iter.next().unwrap();
        });

        // Parse headers.
        let mut bytes_lens = Vec::<u64>::with_capacity(number_of_data_items);
        let mut ids = vec![Vec::<u8>::with_capacity(32); 2];
        (0..number_of_data_items).for_each(|i| {
            let result = [(); 8].map(|_| bundle_iter.next().unwrap());
            bytes_lens.push(u64::from_le_bytes(result));
            (0..24).for_each(|_| {
                bundle_iter.next().unwrap();
            });
            (0..32).for_each(|_| {
                ids[i].push(bundle_iter.next().unwrap());
            });
        });

        // Parse data_items - data_item verified during deserialization
        let mut bytes_lens_iter = bytes_lens.into_iter();
        let mut ids_iter = ids.into_iter();
        let data_items: Result<Vec<DataItem>, _> = (0..number_of_data_items)
            .map(|_| {
                let bytes_len = bytes_lens_iter.next().unwrap() as usize;
                let mut bytes_vec = Vec::<u8>::with_capacity(bytes_len);
                (0..bytes_len).for_each(|_| bytes_vec.push(bundle_iter.next().unwrap()));
                let mut data_item = DataItem::deserialize(bytes_vec)?;

                let deep_hash = self
                    .crypto
                    .deep_hash(data_item.to_deep_hash_item()?)
                    .unwrap();
                self.crypto
                    .verify(&data_item.signature.0, &deep_hash)
                    .unwrap();

                data_item.id.0 = ids_iter.next().unwrap();

                Ok(data_item)
            })
            .collect();

        data_items
    }

    pub async fn create_bundle_transaction_from_file_paths<IP>(
        &self,
        paths_iter: IP,
        tags: Vec<Tag<String>>,
        log_dir: Option<PathBuf>,
        price_terms: (u64, u64),
    ) -> Result<Transaction, Error>
    where
        IP: Iterator<Item = PathBuf> + Send,
    {
        let data_items = self
            .create_data_items_from_file_paths(paths_iter, tags, log_dir)
            .await?;

        let bundle = self.create_bundle_from_data_items(data_items)?;
        let other_tags = Some(vec![
            Tag::<Base64>::from_utf8_strs("Bundle-Format", "binary")?,
            Tag::<Base64>::from_utf8_strs("Bundle-Version", "2.0.0")?,
        ]);

        let transaction = self
            .create_transaction(bundle, other_tags, None, price_terms)
            .await?;
        Ok(transaction)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        error::Error,
        transaction::{Base64, FromUtf8Strs, Tag, ToItems},
        utils::{TempDir, TempFrom},
        Arweave, Status,
    };
    use glob::glob;
    use matches::assert_matches;
    use std::{path::PathBuf, str::FromStr};

    #[tokio::test]
    async fn test_cannot_post_unsigned_transaction() -> Result<(), Error> {
        let arweave = Arweave::from_keypair_path(
            PathBuf::from(
                "tests/fixtures/arweave-key-7eV1qae4qVNqsNChg3Scdi-DpOLJPCogct4ixoq1WNg.json",
            ),
            None,
        )
        .await?;

        let file_path = PathBuf::from("tests/fixtures/0.png");
        let last_tx = Base64::from_str("LCwsLCwsLA")?;
        let other_tags = vec![Tag::<Base64>::from_utf8_strs("key2", "value2")?];
        let transaction = arweave
            .create_transaction_from_file_path(file_path, Some(other_tags), Some(last_tx), (0, 0))
            .await?;

        let error = arweave
            .post_transaction(&transaction, None)
            .await
            .unwrap_err();
        assert_matches!(error, Error::UnsignedTransaction);

        Ok(())
    }

    #[tokio::test]
    async fn test_create_write_read_status() -> Result<(), Error> {
        let arweave = Arweave::from_keypair_path(
            PathBuf::from(
                "tests/fixtures/arweave-key-7eV1qae4qVNqsNChg3Scdi-DpOLJPCogct4ixoq1WNg.json",
            ),
            None,
        )
        .await?;

        let file_path = PathBuf::from("tests/fixtures/0.png");
        let last_tx = Base64::from_str("LCwsLCwsLA")?;
        let other_tags = vec![Tag::<Base64>::from_utf8_strs("key2", "value2")?];
        let transaction = arweave
            .create_transaction_from_file_path(
                file_path.clone(),
                Some(other_tags),
                Some(last_tx),
                (0, 0),
            )
            .await?;

        let signed_transaction = arweave.sign_transaction(transaction)?;

        let status = Status {
            id: signed_transaction.id.clone(),
            reward: signed_transaction.reward,
            file_path: Some(file_path.clone()),
            ..Default::default()
        };

        let temp_log_dir = TempDir::from_str("./tests/").await?;
        let log_dir = temp_log_dir.0.clone();

        arweave
            .write_status(status.clone(), log_dir.clone())
            .await?;

        let read_status = arweave.read_status(file_path, log_dir).await?;

        assert_eq!(status, read_status);

        Ok(())
    }

    #[tokio::test]
    async fn test_deserialize_bundle() -> Result<(), Error> {
        let arweave = Arweave::from_keypair_path(
            PathBuf::from(
                "tests/fixtures/arweave-key-7eV1qae4qVNqsNChg3Scdi-DpOLJPCogct4ixoq1WNg.json",
            ),
            None,
        )
        .await?;

        let paths_iter = glob("tests/fixtures/[0-1]*.png")?.filter_map(Result::ok);
        let pre_data_items = arweave
            .create_data_items_from_file_paths(paths_iter, Vec::new(), None)
            .await?;

        let data_item_lens: Vec<usize> = pre_data_items.iter().map(|d| d.data.0.len()).collect();
        let bundle = arweave.create_bundle_from_data_items(pre_data_items.clone())?;

        // 2 items in the bundle
        assert_eq!(u64::from_le_bytes(bundle[0..8].try_into().unwrap()), 2);

        // 2892 bytes in the first item
        assert_eq!(u64::from_le_bytes(bundle[32..40].try_into().unwrap()), 2892);

        // 2978 bytes in the secpnd item
        assert_eq!(
            u64::from_le_bytes(bundle[96..104].try_into().unwrap()),
            2978
        );

        // signature type is 1
        assert_eq!(
            u16::from_le_bytes(bundle[160..162].try_into().unwrap()),
            1u16
        );

        // owner is same as signer
        assert_eq!(
            &bundle[(162 + 512)..(162 + 1024)],
            &arweave.crypto.keypair_modulus().unwrap().0
        );

        // anchor is present
        assert_eq!(bundle[160 + 2 + 1024 + 1], 0);

        // number of tags is 2
        assert_eq!(
            u64::from_le_bytes(
                bundle[(160 + 2 + 1024 + 1 + 1)..(160 + 2 + 1024 + 1 + 1 + 8)]
                    .try_into()
                    .unwrap()
            ),
            2u64
        );

        // number of tag bytes is 52
        assert_eq!(
            u64::from_le_bytes(
                bundle[(160 + 2 + 1024 + 1 + 1 + 8)..(160 + 2 + 1024 + 1 + 1 + 8 + 8)]
                    .try_into()
                    .unwrap()
            ),
            52u64
        );

        // sig type of second item is 1
        assert_eq!(
            u16::from_le_bytes(
                bundle[(160 + 2 + 1024 + 1 + 1 + 8 + 8 + 52 + &data_item_lens[0])
                    ..(160 + 2 + 1024 + 1 + 1 + 8 + 8 + 52 + &data_item_lens[0] + 2)]
                    .try_into()
                    .unwrap()
            ),
            1u16
        );

        // data items verify and deserialize
        let post_data_items = arweave.deserialize_bundle(bundle)?;
        assert_eq!(post_data_items.len(), 2);
        let mut pre_data_items_iter = pre_data_items.into_iter();
        let mut post_data_items_iter = post_data_items.into_iter();

        // deep hash items and signatures are the same and singatures verify
        for _ in 0..2 {
            let pre_data_item = pre_data_items_iter.next().unwrap();
            let post_data_item = post_data_items_iter.next().unwrap();
            let pre_deep_hash = arweave
                .crypto
                .deep_hash(pre_data_item.to_deep_hash_item().unwrap())
                .unwrap();
            let post_deep_hash = arweave
                .crypto
                .deep_hash(post_data_item.to_deep_hash_item().unwrap())
                .unwrap();
            assert_eq!(pre_deep_hash, post_deep_hash);
            assert_eq!(pre_data_item.signature, post_data_item.signature);
            arweave
                .crypto
                .verify(&pre_data_item.signature.0, &pre_deep_hash)?;
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_price_points() -> Result<(), Error> {
        let mut price = 0 as u64;
        println!("{:>6}  {:>12} {:>12}", "size", "winstons", "incremental");
        println!("{:-<40}", "");
        for p in 1..10 {
            let size = p * 100 * 256;
            let new_price = reqwest::get(format!("https://arweave.net/price/{}", size * 1024))
                .await?
                .json::<u64>()
                .await?;
            println!("{:>6}k {:>12} {:>12}", size, new_price, new_price - price);
            price = new_price;
        }
        Ok(())
    }
}
