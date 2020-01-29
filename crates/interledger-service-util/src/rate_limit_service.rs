use async_trait::async_trait;
use interledger_packet::{ErrorCode, RejectBuilder};
use interledger_service::{Account, AddressStore, IlpResult, IncomingRequest, IncomingService};
use log::{error, warn};
use std::marker::PhantomData;

/// Extension trait for [`Account`](../interledger_service/trait.Account.html) with rate limiting related information
pub trait RateLimitAccount: Account {
    /// The maximum packets per minute allowed for this account
    fn packets_per_minute_limit(&self) -> Option<u32> {
        None
    }

    /// The maximum units per minute allowed for this account
    fn amount_per_minute_limit(&self) -> Option<u64> {
        None
    }
}

/// Rate limiting related errors
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RateLimitError {
    /// Account exceeded their packet limit
    PacketLimitExceeded,
    /// Account exceeded their amount limit
    ThroughputLimitExceeded,
    /// There was an internal error when trying to connect to the store
    StoreError,
}

/// Store trait which manages the rate limit related information of accounts
#[async_trait]
pub trait RateLimitStore {
    /// The provided account must implement [`RateLimitAccount`](./trait.RateLimitAccount.html)
    type Account: RateLimitAccount;

    /// Apply rate limits based on the packets per minute and amount of per minute
    /// limits set on the provided account
    async fn apply_rate_limits(
        &self,
        account: Self::Account,
        prepare_amount: u64,
    ) -> Result<(), RateLimitError>;

    /// Refunds the throughput limit which was charged to an account
    /// Called if the node receives a reject packet after trying to forward
    /// a packet to a peer, meaning that effectively reject packets do not
    /// count towards a node's throughput limits
    async fn refund_throughput_limit(
        &self,
        account: Self::Account,
        prepare_amount: u64,
    ) -> Result<(), ()>;
}

/// # Rate Limit Service
///
/// Incoming Service responsible for rejecting requests
/// by users who have reached their account's rate limit.
/// Talks with the associated Store in order to figure out
/// and set the rate limits per account.
/// This service does packet based limiting and amount based limiting.
///
/// Forwards everything else.
/// Requires a `RateLimitAccount` and a `RateLimitStore`.
/// It is an IncomingService.
#[derive(Clone)]
pub struct RateLimitService<S, I, A> {
    store: S,
    next: I, // Can we somehow omit the PhantomData
    account_type: PhantomData<A>,
}

impl<S, I, A> RateLimitService<S, I, A>
where
    S: AddressStore + RateLimitStore<Account = A> + Clone + Send + Sync,
    I: IncomingService<A> + Clone + Send + Sync, // Looks like 'static is not required?
    A: RateLimitAccount + Sync,
{
    pub fn new(store: S, next: I) -> Self {
        RateLimitService {
            store,
            next,
            account_type: PhantomData,
        }
    }
}

#[async_trait]
impl<S, I, A> IncomingService<A> for RateLimitService<S, I, A>
where
    S: AddressStore + RateLimitStore<Account = A> + Clone + Send + Sync + 'static,
    I: IncomingService<A> + Clone + Send + Sync + 'static,
    A: RateLimitAccount + Sync + 'static,
{
    /// On receiving a request:
    /// 1. Apply rate limit based on the sender of the request and the amount in the prepare packet in the request
    /// 1. If no limits were hit forward the request
    ///     - If it succeeds, OK
    ///     - If the request forwarding failed, the client should not be charged towards their throughput limit, so they are refunded, and return a reject
    /// 1. If the limit was hit, return a reject with the appropriate ErrorCode.
    async fn handle_request(&mut self, request: IncomingRequest<A>) -> IlpResult {
        let ilp_address = self.store.get_ilp_address();
        let mut next = self.next.clone();
        let store = self.store.clone();
        let account = request.from.clone();
        let account_clone = account.clone();
        let prepare_amount = request.prepare.amount();
        let has_throughput_limit = account.amount_per_minute_limit().is_some();
        // request.from and request.amount are used for apply_rate_limits, can't the previous service
        // always set the account to have None for both?
        match self
            .store
            .apply_rate_limits(request.from.clone(), request.prepare.amount())
            .await
        {
            Ok(_) => {
                let packet = next.handle_request(request).await;
                // If we did not get a fulfill, we should refund the sender
                if packet.is_err() && has_throughput_limit {
                    let refunded = store
                        .refund_throughput_limit(account_clone, prepare_amount)
                        .await;
                    // if refunding failed, that's too bad, we will just return the reject
                    // from the peer
                    if let Err(err) = refunded {
                        error!("Error refunding throughput limit: {:?}", err);
                    }
                }

                // return the packet
                packet
            }
            Err(err) => {
                let code = match err {
                    RateLimitError::PacketLimitExceeded => {
                        if let Some(limit) = account.packets_per_minute_limit() {
                            warn!("Account {} was rate limited for sending too many packets. Limit is: {} per minute", account.id(), limit);
                        }
                        ErrorCode::T05_RATE_LIMITED
                    }
                    RateLimitError::ThroughputLimitExceeded => {
                        if let Some(limit) = account.amount_per_minute_limit() {
                            warn!("Account {} was throughput limited for trying to send too much money. Limit is: {} per minute", account.id(), limit);
                        }
                        ErrorCode::T04_INSUFFICIENT_LIQUIDITY
                    }
                    RateLimitError::StoreError => ErrorCode::T00_INTERNAL_ERROR,
                };

                let reject = RejectBuilder {
                    code,
                    triggered_by: Some(&ilp_address),
                    message: &[],
                    data: &[],
                }
                .build();

                Err(reject)
            }
        }
    }
}
