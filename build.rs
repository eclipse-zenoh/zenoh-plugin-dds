fn main() {
    let link_search_path: String = 
        if let Ok(path) = std::env::var("CYCLONE_LIB") { format!("cargo:rustc-link-search={}", path) }
        else { "cargo:rustc-link-search=/usr/local/lib".into() };
    println!("{}", link_search_path);
    println!("cargo:rustc-link-lib=cdds-util");
}
