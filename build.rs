fn main() {
    println!("cargo:rerun-if-changed=resources.rc");
    embed_resource::compile("resources.rc", embed_resource::NONE);
}
