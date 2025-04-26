use crate::types::{Transaction, TransactionListBuilder, VaultAccounts};
use crate::{Client, Epoch, FireblocksError, PagingVaultRequestBuilder, ParamError, QueryParams, Result};
use chrono::{TimeZone, Utc};
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::{FutureExt, Stream, StreamExt};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

#[derive(Clone)]
pub struct PagedClient {
  pub client: Arc<Client>,
}

pub struct VaultStream {
  client: Arc<Client>,
  batch: u16,
  after: Option<String>,
  init: bool,
  fut: FuturesUnordered<BoxFuture<'static, Result<VaultAccounts>>>,
}

impl VaultStream {
  fn new(client: Arc<Client>, batch: u16) -> Self {
    Self { client, batch, init: false, after: None, fut: FuturesUnordered::new() }
  }
  fn build_params(&self) -> std::result::Result<QueryParams, ParamError> {
    PagingVaultRequestBuilder::new().limit(self.batch).after(self.after.as_ref().unwrap_or(&String::new())).build()
  }
}

pub struct TransactionStream {
  client: Arc<Client>,
  batch: u16,
  init: bool, // has the stream started?
  vault_id: i32,
  after: Epoch,
  is_source: bool, // are we streaming from source vault account or destination
  fut: FuturesUnordered<BoxFuture<'static, Result<Vec<Transaction>>>>,
}

impl TransactionStream {
  fn from_source(client: Arc<Client>, batch: u16, vault_id: i32, after: Epoch) -> Self {
    Self { client, batch, init: false, vault_id, after, fut: FuturesUnordered::new(), is_source: true }
  }

  fn from_dest(client: Arc<Client>, batch: u16, vault_id: i32, after: Epoch) -> Self {
    Self { client, batch, init: false, vault_id, after, fut: FuturesUnordered::new(), is_source: false }
  }

  fn build_params(&self) -> std::result::Result<QueryParams, ParamError> {
    let mut builder = TransactionListBuilder::new();
    let builder = builder.limit(self.batch).sort_asc().order_created_at().after(&self.after);

    if self.is_source {
      return builder.source_id(self.vault_id).build();
    }
    builder.destination_id(self.vault_id).build()
  }
}

pub trait AsyncIteratorAsyncNext {
  type Item;
  async fn next(&mut self) -> Result<Option<Self::Item>>;
}

impl Stream for VaultStream {
  type Item = Result<VaultAccounts>;

  #[allow(clippy::cognitive_complexity)]
  fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
    if !self.init {
      tracing::debug!("init future");
      self.init = true;
      let client = self.client.clone();
      let params = match self.build_params() {
        Ok(p) => p,
        Err(e) => return Poll::Ready(Some(Err(FireblocksError::from(e)))),
      };
      let fut = async move { client.vaults(params).await }.boxed();
      self.fut.push(fut);
      cx.waker().wake_by_ref();
      return Poll::Pending;
    }

    // Try to resolve any existing futures first
    tracing::trace!("check future poll");
    match self.fut.poll_next_unpin(cx) {
      Poll::Ready(opt) => {
        if let Some(result) = opt {
          match result {
            Ok((ref va, ref _id)) => {
              self.after.clone_from(&va.paging.after);
            },
            Err(e) => {
              return Poll::Ready(Some(Err(e)));
            },
          }
          return Poll::Ready(Some(result));
        }
      },
      Poll::Pending => {
        tracing::trace!("still pending");
        cx.waker().wake_by_ref();
        return Poll::Pending;
      },
    };

    tracing::trace!("checking after {:#?}", self.after);
    // If there are no more pages to fetch and no pending futures, end the stream
    if self.after.is_none() {
      return Poll::Ready(None);
    }

    let client = self.client.clone();
    let params = match self.build_params() {
      Ok(p) => p,
      Err(e) => return Poll::Ready(Some(Err(FireblocksError::from(e)))),
    };
    let fut = async move { client.vaults(params).await }.boxed();
    self.fut.push(fut);
    cx.waker().wake_by_ref();
    Poll::Pending
  }
}

impl Stream for TransactionStream {
  type Item = Result<Vec<Transaction>>;

  fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
    if !self.init {
      tracing::debug!("init tracing stream");
      self.init = true;
      let client = self.client.clone();
      let params = match self.build_params() {
        Ok(p) => p,
        Err(e) => return Poll::Ready(Some(Err(FireblocksError::from(e)))),
      };
      let fut = async move { client.transactions(params).await }.boxed();
      self.fut.push(fut);
      cx.waker().wake_by_ref();
      return Poll::Pending;
    }

    match self.fut.poll_next_unpin(cx) {
      Poll::Ready(opt) => {
        if let Some(result) = opt {
          match result {
            Ok((ref va, ref _id)) => {
              if va.is_empty() {
                return Poll::Ready(None);
              }
              if let Some(last) = va.last() {
                tracing::trace!("1st after {:#?} last after {:#?}", va[0].created_at, last.created_at);
                self.after = last.created_at + chrono::Duration::milliseconds(1);
              }
            },
            Err(e) => {
              return Poll::Ready(Some(Err(e)));
            },
          }
          return Poll::Ready(Some(result));
        }
      },
      Poll::Pending => {
        cx.waker().wake_by_ref();
        return Poll::Pending;
      },
    };

    let client = self.client.clone();
    let params = match self.build_params() {
      Ok(p) => p,
      Err(e) => return Poll::Ready(Some(Err(FireblocksError::from(e)))),
    };
    let fut = async move { client.transactions(params).await }.boxed();
    self.fut.push(fut);
    cx.waker().wake_by_ref();
    Poll::Pending
  }
}

impl PagedClient {
  pub const fn new(client: Arc<Client>) -> Self {
    Self { client }
  }

  /// Stream the vault accounts based on batch size
  ///
  /// ```
  /// use std::sync::Arc;
  /// use futures::TryStreamExt;
  /// use fireblocks_sdk::{Client, PagedClient};
  ///
  /// async fn vault_accounts(c: Client) -> color_eyre::Result<()> {
  ///   let pc = PagedClient::new(Arc::new(c));
  ///   let mut vault_stream = pc.vaults(100);
  ///   while let Ok(Some(result)) = vault_stream.try_next().await {
  ///     tracing::info!("accounts {}", result.0.accounts.len());
  ///     tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
  ///    }
  ///   Ok(())
  /// }
  /// ```
  /// see [`Client::vaults`]
  pub fn vaults(&self, batch_size: u16) -> VaultStream {
    VaultStream::new(self.client.clone(), batch_size)
  }

  /// Stream all the transactions from source vault account id and after some date
  ///
  /// Default date is 2022-04-06 if None provided
  ///
  /// ```
  /// use std::sync::Arc;
  /// use futures::TryStreamExt;
  /// use fireblocks_sdk::{Client, PagedClient};
  ///
  /// async fn transactions_paged(c: Client) -> color_eyre::Result<()> {
  ///   let pc = PagedClient::new(Arc::new(c));
  ///   let mut ts = pc.transactions_from_source(0, 100, None);
  ///   while let Ok(Some(result)) = ts.try_next().await {
  ///     tracing::info!("transactions {}", result.0.len());
  ///    }
  ///   Ok(())
  /// }
  /// ```
  ///
  /// see
  /// * [`Client::transactions`]
  pub fn transactions_from_source(&self, vault_id: i32, batch_size: u16, after: Option<Epoch>) -> TransactionStream {
    let default_after = Utc.with_ymd_and_hms(2022, 4, 6, 0, 1, 1).unwrap();
    TransactionStream::from_source(self.client.clone(), batch_size, vault_id, after.unwrap_or(default_after))
  }

  ///  Stream all the transactions from destination vault account id
  ///  See [`self.transactions_from_source`]
  pub fn transactions_from_destination(
    &self,
    vault_id: i32,
    batch_size: u16,
    after: Option<Epoch>,
  ) -> TransactionStream {
    let default_after = Utc.with_ymd_and_hms(2022, 4, 6, 0, 1, 1).unwrap();
    TransactionStream::from_dest(self.client.clone(), batch_size, vault_id, after.unwrap_or(default_after))
  }
}
