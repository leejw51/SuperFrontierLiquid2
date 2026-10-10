fn main() {
    capnpc::CompilerCommand::new()
        .src_prefix("schema")
        .file("schema/types.capnp")
        .file("schema/tx.capnp")
        .file("schema/block.capnp")
        .file("schema/rpc.capnp")
        .file("schema/p2p.capnp")
        .run()
        .expect("capnp schema compile failed");
    println!("cargo:rerun-if-changed=schema");
}
