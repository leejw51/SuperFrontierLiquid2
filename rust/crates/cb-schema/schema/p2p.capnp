@0xa1b2c3d4e5f60005;

# Peer-to-peer gossip wire format.
#
# This file is additive and self-contained: it shares no ordinals with
# block.capnp, tx.capnp or rpc.capnp, and it carries no consensus data of its
# own. Block and transaction bodies travel as opaque `Data` holding the
# canonical bincode encodings those types already use, so the gossip layer
# cannot accidentally become a second, divergent definition of what a block is.
#
# Cap'n Proto's evolution rule applies here as everywhere: append fields, never
# reorder, never renumber, never repurpose.

# An IP endpoint. `ip` is 4 bytes for IPv4 and 16 for IPv6; any other length is
# a decode error, which is how a malformed address is rejected rather than
# silently coerced.
struct Endpoint {
  ip   @0 :Data;
  port @1 :UInt16;
}

# A peer as gossiped in a `peers` response.
struct NodeAddr {
  nodeId   @0 :Data;      # 20 bytes
  endpoint @1 :Endpoint;
}

# The handshake, sent in both directions immediately on connect.
struct Hello {
  protocol    @0 :UInt32;
  chainId     @1 :UInt32;
  genesis     @2 :Data;   # 32-byte chain fingerprint
  nodeId      @3 :Data;   # 20 bytes, proven by `sig`
  software    @4 :Text;
  headHeight  @5 :UInt64;
  hasListen   @6 :Bool;
  listen      @7 :Endpoint;
  nonce       @8 :UInt64;
  sig         @9 :Data;   # 65-byte secp256k1 over the canonical preimage
}

# Why a connection is being closed. Kept as an enum so an operator reading a
# packet capture sees a reason rather than a number.
enum DisconnectCode {
  chainIdMismatch     @0;
  genesisMismatch     @1;
  versionMismatch     @2;
  selfConnection      @3;
  duplicateConnection @4;
  badIdentity         @5;
  banned              @6;
  misbehaving         @7;
  tooManyPeers        @8;
  protocolViolation   @9;
  timeout             @10;
  shutdown            @11;
}

# One block, as height plus canonical `Block::encode` bytes.
struct BlockEntry {
  height @0 :UInt64;
  bytes  @1 :Data;
}

struct BlockAnnounce {
  hash   @0 :Data;   # 32 bytes
  height @1 :UInt64;
}

struct HeightRange {
  from @0 :UInt64;
  to   @1 :UInt64;
}

# One message on the wire.
struct Envelope {
  union {
    hello          @0  :Hello;
    disconnect     @1  :DisconnectCode;
    getPeers       @2  :Void;
    peers          @3  :List(NodeAddr);
    blockAnnounce  @4  :BlockAnnounce;
    getBlock       @5  :Data;              # 32-byte block hash
    blockBody      @6  :BlockEntry;
    txAnnounce     @7  :Data;              # 32-byte tx hash
    getTx          @8  :Data;              # 32-byte tx hash
    txBody         @9  :Data;              # canonical SignedTx bincode
    getBlockRange  @10 :HeightRange;
    blockRange     @11 :List(BlockEntry);
    ping           @12 :UInt64;
    pong           @13 :UInt64;
    # A consensus-protocol message between validators, opaque to gossip:
    # `cb_consensus::bft::Message` bincode. Delivered to the node's consensus
    # driver, never verified, stored or forwarded by the p2p layer.
    consensus      @14 :Data;
  }
}
