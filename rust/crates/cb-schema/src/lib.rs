//! Cap'n Proto schemas + canonical-encoding helpers.

#![allow(clippy::all)]
#![allow(non_snake_case)]
#![allow(dead_code)]
// capnpc's generated readers wrap some types in parentheses; that is the
// generator's business, not this crate's.
#![allow(unused_parens)]

#[allow(clippy::all, non_snake_case, dead_code, unused_parens)]
pub mod types_capnp {
    include!(concat!(env!("OUT_DIR"), "/types_capnp.rs"));
}
#[allow(clippy::all, non_snake_case, dead_code, unused_parens)]
pub mod tx_capnp {
    include!(concat!(env!("OUT_DIR"), "/tx_capnp.rs"));
}
#[allow(clippy::all, non_snake_case, dead_code, unused_parens)]
pub mod block_capnp {
    include!(concat!(env!("OUT_DIR"), "/block_capnp.rs"));
}
#[allow(clippy::all, non_snake_case, dead_code, unused_parens)]
pub mod rpc_capnp {
    include!(concat!(env!("OUT_DIR"), "/rpc_capnp.rs"));
}

#[allow(clippy::all, non_snake_case, dead_code, unused_parens)]
pub mod p2p_capnp {
    include!(concat!(env!("OUT_DIR"), "/p2p_capnp.rs"));
}

pub mod canonical;
