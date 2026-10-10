@0xa1b2c3d4e5f60004;

using import "types.capnp".Address;
using import "types.capnp".AssetId;
using import "types.capnp".DerivId;
using import "types.capnp".H256;
using import "types.capnp".MarketId;
using import "types.capnp".Side;
using import "types.capnp".SignedI128;
using import "types.capnp".U128;
using import "tx.capnp".SignedTx;
using import "block.capnp".Block;

enum SubmitErrorCode {
  badSignature @0;
  badNonce @1;
  insufficientBalance @2;
  postOnlyWouldCross @3;
  unknownMarket @4;
  invalidAmount @5;
  notAuthorized @6;
  mempoolFull @7;
  internal @8;
}

struct SubmitError {
  code @0 :SubmitErrorCode;
  message @1 :Text;
}

struct SubmitResult {
  union {
    ok @0 :Void;
    rejected @1 :SubmitError;
  }
}

struct BalanceEntry {
  asset @0 :AssetId;
  amount @1 :U128;
}

struct ChainInfo {
  chainId @0 :UInt32;
  denom   @1 :Text;
  # The head at the moment of the call, and the lowest height whose body
  # this node still holds. A streaming client that asks to replay from below
  # `oldestBlock` cannot be served the missing range; `streamBlocks` starts
  # it at `oldestBlock` instead, and this is how the client finds out.
  headHeight  @2 :UInt64;
  oldestBlock @3 :UInt64;
}

struct AccountState {
  nonce @0 :UInt64;
  balances @1 :List(BalanceEntry);
  locked @2 :List(BalanceEntry);
}

struct OrderBookLevel {
  price @0 :U128;
  qty @1 :U128;
}

struct OrderBookSnapshot {
  market @0 :MarketId;
  bids @1 :List(OrderBookLevel);
  asks @2 :List(OrderBookLevel);
  asOfHeight @3 :UInt64;
}

struct MarketEvent {
  union {
    fill @0 :Fill;
    rest @1 :Rest;
    cancel @2 :Cancel;
  }
  struct Fill   { takerSide @0 :UInt8; price @1 :U128; qty @2 :U128; height @3 :UInt64; }
  struct Rest   { price @0 :U128; qty @1 :U128; side @2 :UInt8; height @3 :UInt64; }
  struct Cancel { price @0 :U128; qty @1 :U128; side @2 :UInt8; height @3 :UInt64; }
}

struct Vote {
  view      @0 :UInt64;
  height    @1 :UInt64;
  blockHash @2 :H256;
  validator @3 :Address;
  sig       @4 :Data;
}

enum VoteErrorCode {
  unknownValidator @0;
  malformedSig @1;
  signerMismatch @2;
  staleHeight @3;
  unknownBlock @4;
  duplicate @5;
  internal @6;
}

struct VoteError {
  code @0 :VoteErrorCode;
  message @1 :Text;
}

struct VoteResult {
  union {
    accepted @0 :Void;  # vote stored, quorum not yet reached
    quorum   @1 :Void;  # vote pushed pool to quorum, QC formed
    rejected @2 :VoteError;
  }
}

struct BlockBytesEnvelope {
  # Cap'n Proto-serialised full Block (framed message). On the fast-mirror
  # path the leader re-encodes its in-memory Block as capnp once per stream
  # tick and ships those bytes; the replica writes them verbatim into
  # CF_MIRROR_BLOCKS without a deep decode.
  bytes      @0 :Data;
  height     @1 :UInt64;
  hash       @2 :Data;     # 32 bytes
  stateRoot  @3 :Data;     # 32 bytes
  parentQc   @4 :Data;     # bincode-encoded QuorumCert, may be empty
  view       @5 :UInt64;
  txCount    @6 :UInt32;   # for dashboard metrics
  blocktime  @7 :UInt64;   # header.timestamp, surfaced so the replica can
                           # store it without decoding `bytes`
}

# ---------- Derivatives read-side surfaces (M6) ----------
#
# All structs below mirror the on-chain types from `cb-types::derivatives`.
# Field numbering is fixed; new optional fields go at the end. Cap'n Proto's
# additive evolution rule applies — never reorder, never repurpose, never
# renumber.

# Open position on a single derivative market.
struct Position {
  owner               @0 :Address;
  market              @1 :DerivId;
  side                @2 :Side;
  qty                 @3 :U128;
  entryFundingIndex   @4 :SignedI128;
  entryPxQ8           @5 :U128;
  marginMode          @6 :UInt8;     # 0 = cross, 1 = isolated
  isolatedMargin      @7 :U128;
  lastFundingHeight   @8 :UInt64;
}

# Per-owner USD collateral pool that backs every cross-margin position.
struct MarginAccount {
  owner          @0 :Address;
  collateral     @1 :U128;
  unrealizedPnl  @2 :SignedI128;
  badDebt        @3 :U128;
}

# Per-perp cumulative funding index plus latest 1h rate.
struct FundingIndex {
  market           @0 :DerivId;
  cumulative       @1 :SignedI128;
  lastTickHeight   @2 :UInt64;
  rateQ9           @3 :Int64;
}

# Last oracle-authority-signed price for a derivative market.
struct OraclePrice {
  market         @0 :DerivId;
  pxQ8           @1 :U128;
  sourceHeight   @2 :UInt64;
  authority      @3 :Address;
}

# Engine-derived mark price.
struct MarkPrice {
  market              @0 :DerivId;
  pxQ8                @1 :U128;
  computedAtHeight    @2 :UInt64;
  premiumEmaQ9        @3 :Int64;
}

# Per-market parameters for a perpetual contract.
struct PerpMarket {
  id                       @0  :DerivId;
  underlying               @1  :AssetId;
  tickSize                 @2  :U128;
  lotSize                  @3  :U128;
  maxLeverageX100          @4  :UInt32;
  initialMarginBps         @5  :UInt16;
  maintenanceMarginBps     @6  :UInt16;
  takerFeeBps              @7  :UInt16;
  makerFeeBpsSigned        @8  :Int16;
  fundingIntervalBlocks    @9  :UInt32;
  fundingRateCapQ9         @10 :Int64;
  oracleAuthority          @11 :Address;
  maxPositionNotional      @12 :U128;
  paused                   @13 :Bool;
  lastTradePxQ8            @14 :U128;
  lastTradeHeight          @15 :UInt64;
  # M4 reduce-only halt flag. Defaults to false on encoders that haven't
  # observed an insurance-fund exhaustion. Additive; never reordered.
  pausedReduceOnlyOnly     @16 :Bool;
}

# Per-market parameters for a dated cash-settled future.
struct FutureMarket {
  id                          @0  :DerivId;
  underlying                  @1  :AssetId;
  tickSize                    @2  :U128;
  lotSize                     @3  :U128;
  maxLeverageX100             @4  :UInt32;
  initialMarginBps            @5  :UInt16;
  maintenanceMarginBps        @6  :UInt16;
  takerFeeBps                 @7  :UInt16;
  makerFeeBpsSigned           @8  :Int16;
  expiryHeight                @9  :UInt64;
  settlementMethod            @10 :UInt8;
  settlementWindowBlocks      @11 :UInt32;
  oracleAuthority             @12 :Address;
  paused                      @13 :Bool;
  # `final_settlement_px_q8` is `Option<u128>` in Rust. Encoded as an
  # explicit (has, value) pair so the wire form stays canonical.
  hasFinalSettlementPx        @14 :Bool;
  finalSettlementPxQ8         @15 :U128;
  # M5: matcher-side last-trade plumbing for futures. Defaults to 0/0 on
  # encoders that haven't observed a fill. Additive; never reordered.
  lastTradePxQ8               @16 :U128;
  lastTradeHeight             @17 :UInt64;
  # Core review L4: reduce-only halt for futures, mirroring the perp flag.
  # Additive; defaults to false on older encoders.
  pausedReduceOnlyOnly        @18 :Bool;
}

# One row of the liquidation queue.
struct LiquidationEntry {
  owner                 @0 :Address;
  market                @1 :DerivId;
  triggeredBy           @2 :Address;
  triggeredAtHeight     @3 :UInt64;
  markAtTriggerQ8       @4 :U128;
  seq                   @5 :UInt64;
}

# A perp orderbook level: one price + total qty.
struct PerpBookLevel {
  priceQ8 @0 :U128;
  qty     @1 :U128;
}

# Snapshot of a perp book up to `depth` levels per side. Bids are returned
# best-first (highest price first); asks best-first (lowest price first).
struct PerpOrderBookSnapshot {
  market         @0 :DerivId;
  bids           @1 :List(PerpBookLevel);
  asks           @2 :List(PerpBookLevel);
  asOfHeight     @3 :UInt64;
}

# A spot market's parameters, as genesis listed them. `tickSize` is in the
# market's own wire price scale (quote decimals + 8 - base decimals) and
# `minQty` in the base asset's scale: exactly the units the executor checks
# `price % tick_size` and `qty % min_qty` in, so a client that reads these can
# build an order the chain will not refuse.
struct SpotMarket {
  id        @0 :MarketId;
  base      @1 :AssetId;
  quote     @2 :AssetId;
  tickSize  @3 :U128;
  minQty    @4 :U128;
  feeBps    @5 :UInt16;
}

# A transaction the sequencer refused. Under the sequencer scheme a refused
# transaction is dropped from the block rather than recorded on chain, so
# this stream is the only way a client that was told "accepted" at admission
# learns that its transaction died, and why. Under BFT the transaction stays
# in the block with `applied` false and this stream is not served.
struct RejectedTx {
  seq     @0 :UInt64;  # position in the node's log, strictly increasing
  height  @1 :UInt64;  # the block it was dropped from; 0 when no block came of the batch
  hash    @2 :H256;
  sender  @3 :Address;
  nonce   @4 :UInt64;
  reason  @5 :Text;    # the executor's error, rendered
}

interface SuperFrontierLiquid {
  submitTx        @0 (tx :SignedTx) -> (result :SubmitResult);
  getAccount      @1 (addr :Address) -> (state :AccountState);
  getBlock        @2 (height :UInt64) -> (block :Block);
  getOrderBook    @3 (market :MarketId, depth :UInt32) -> (snapshot :OrderBookSnapshot);
  streamBlocks    @4 (fromHeight :UInt64) -> (sub :BlockSub);
  streamMarket    @5 (market :MarketId) -> (sub :MarketSub);
  submitVote      @6 (vote :Vote) -> (result :VoteResult);
  # Fast-mirror replica path: stream pre-encoded bincode block bytes plus a
  # tiny envelope of metadata. Replicas write the bytes verbatim to their
  # Blocks CF, skipping a Cap'n Proto deep-decode + bincode re-encode.
  streamBlocksRaw @7 (fromHeight :UInt64) -> (sub :BlockBytesSub);
  chainInfo       @8 () -> (info :ChainInfo);

  # ---------- Derivatives reads (M6) ----------
  getPositions       @9  (addr :Address) -> (positions :List(Position));
  getMarginAccount   @10 (addr :Address) -> (margin :MarginAccount);
  getFundingIndex    @11 (market :DerivId) -> (index :FundingIndex);
  getOraclePrice     @12 (market :DerivId) -> (oracle :OraclePrice);
  getMarkPrice       @13 (market :DerivId) -> (mark :MarkPrice);
  getPerpMarket      @14 (market :DerivId) -> (perp :PerpMarket);
  listPerpMarkets    @15 () -> (markets :List(PerpMarket));
  getFutureMarket    @16 (market :DerivId) -> (future :FutureMarket);
  listFutureMarkets  @17 () -> (markets :List(FutureMarket));
  getLiquidationQueue @18 (market :DerivId) -> (entries :List(LiquidationEntry));
  getPerpOrderBook   @19 (market :DerivId, depth :UInt32) -> (snapshot :PerpOrderBookSnapshot);

  # ---------- Spot market configuration ----------
  # The gateway publishes tick and lot sizes to the browser and rounds a
  # market order's protective price to the tick. Before this method it had
  # to guess them, and a guess finer than genesis made every market order
  # fail `price % tick_size` at the executor.
  listSpotMarkets    @20 () -> (markets :List(SpotMarket));

  # ---------- Refusals ----------
  # Every transaction the sequencer refused at execution, from `fromSeq` on
  # (0 = from now). Bounded on the node: a subscriber that falls far behind
  # sees a gap in `seq`. A validator answers `unimplemented`.
  streamRejected     @21 (fromSeq :UInt64) -> (sub :RejectedSub);
}

interface BlockSub {
  next @0 () -> (block :Block);
}
interface BlockBytesSub {
  next @0 () -> (env :BlockBytesEnvelope);
}
interface MarketSub {
  next @0 () -> (event :MarketEvent);
}
interface RejectedSub {
  # Blocks until at least one refusal is available.
  next @0 () -> (entries :List(RejectedTx), nextSeq :UInt64);
}
