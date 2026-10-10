@0xa1b2c3d4e5f60002;

using import "types.capnp".H256;
using import "types.capnp".Address;
using import "types.capnp".U128;
using import "types.capnp".AssetId;
using import "types.capnp".MarketId;
using import "types.capnp".Side;
using import "types.capnp".Tif;
using import "types.capnp".DerivId;

struct Transfer {
  to @0 :Address;
  asset @1 :AssetId;
  amount @2 :U128;
}

struct Deposit {
  user @0 :Address;
  asset @1 :AssetId;
  amount @2 :U128;
  externalTxRef @3 :Text;
}

struct Withdraw {
  asset @0 :AssetId;
  amount @1 :U128;
  externalAddress @2 :Text;
}

struct WithdrawSettled {
  withdrawId @0 :H256;
  externalTxRef @1 :Text;
}

struct PlaceOrder {
  market @0 :MarketId;
  side @1 :Side;
  price @2 :U128;
  qty @3 :U128;
  tif @4 :Tif;
  postOnly @5 :Bool;
  clientOrderId @6 :H256;
}

struct CancelOrder { orderId @0 :H256; }
struct BatchCancel { market @0 :MarketId; }

# ---------- Derivatives (M1: shapes only; handlers are M2+) ----------

struct PlacePerpOrder {
  market @0 :DerivId;
  side @1 :Side;
  priceQ8 @2 :U128;
  qty @3 :U128;
  tif @4 :Tif;
  postOnly @5 :Bool;
  reduceOnly @6 :Bool;
  clientOrderId @7 :H256;
}

struct CancelPerpOrder { orderId @0 :H256; }

struct OpenPosition {
  market @0 :DerivId;
  side @1 :Side;
  qty @2 :U128;
  maxSlippageBps @3 :UInt16;
}

struct ClosePosition {
  market @0 :DerivId;
  qty @1 :U128;          # 0 = close all
  maxSlippageBps @2 :UInt16;
}

struct AdjustMargin {
  market @0 :DerivId;
  isTopup @1 :Bool;
  amount @2 :U128;
}

struct LiquidationTrigger {
  owner @0 :Address;
  market @1 :DerivId;
}

struct OracleUpdate {
  market @0 :DerivId;
  pxQ8 @1 :U128;
  submittedHeight @2 :UInt64;
}

struct FundingTick {
  market @0 :DerivId;
}

struct FuturesSettle {
  market @0 :DerivId;
}

struct Transaction {
  nonce @0 :UInt64;
  chainId @1 :UInt32;
  body :union {
    transfer @2 :Transfer;
    deposit @3 :Deposit;
    withdraw @4 :Withdraw;
    withdrawSettled @5 :WithdrawSettled;
    placeOrder @6 :PlaceOrder;
    cancelOrder @7 :CancelOrder;
    batchCancel @8 :BatchCancel;
    # ---- derivatives ----
    placePerpOrder      @9  :PlacePerpOrder;
    cancelPerpOrder     @10 :CancelPerpOrder;
    openPosition        @11 :OpenPosition;
    closePosition       @12 :ClosePosition;
    adjustMargin        @13 :AdjustMargin;
    liquidationTrigger  @14 :LiquidationTrigger;
    oracleUpdate        @15 :OracleUpdate;
    fundingTick         @16 :FundingTick;
    futuresSettle       @17 :FuturesSettle;
  }
}

struct Signature {
  r @0 :Data;
  s @1 :Data;
  recoveryId @2 :UInt8;
}

struct SignedTx {
  tx @0 :Transaction;
  sig @1 :Signature;
}
