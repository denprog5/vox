fn main() {
    // Иконка exe и сведения о версии; веб-страницы и картинки аватара встраиваются через include_*.
    println!("cargo:rerun-if-changed=assets");
    embed_resource::compile("assets/app.rc", embed_resource::NONE)
        .manifest_optional()
        .unwrap();
}
