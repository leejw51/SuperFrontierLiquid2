//! The WebAssembly entry point.
//!
//! Everything lives in the library so that the pure-logic modules — formatting,
//! the order book, backoff, the wallet, form validation — can be tested on the
//! host as well as under `wasm-bindgen-test`.

fn main() {
    cb_web::run();
}
