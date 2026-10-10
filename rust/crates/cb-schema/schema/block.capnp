@0xa1b2c3d4e5f60003;

using import "types.capnp".Address;
using import "types.capnp".H256;
using import "tx.capnp".SignedTx;

struct BlockHeader {
  height @0 :UInt64;
  prevHash @1 :H256;
  txRoot @2 :H256;
  stateRoot @3 :H256;
  timestamp @4 :UInt64;
  sequencerSig @5 :Data;
  view @6 :UInt64;
  # Bincode-serialized QuorumCert proving block (height-1) was finalized.
  # Empty bytes when there is no parent QC yet (block 1 or pre-voting).
  parentQc @7 :Data;
}

struct Block {
  header @0 :BlockHeader;
  txs @1 :List(SignedTx);
  # Pre-recovered senders, parallel to `txs` (same length, same order). Lets
  # followers skip per-tx ECDSA recovery on the hot apply path. Empty for
  # legacy blocks; followers fall back to `verify()` in that case.
  senders @2 :List(Address);
  # Under BFT, parallel to `txs`: whether each transaction executed. A
  # committed payload is kept whole, applied or not, so a reader needs this
  # to tell a refused order from a filled one. Empty under the sequencer
  # scheme, where every transaction in a block executed.
  applied @3 :List(Bool);
}
