fn main() {
    let opt_level = std::env::var("OPT_LEVEL").unwrap_or_else(|_| String::from("unknown"));
    println!("cargo:rustc-env=ZEPPELIN_BENCH_OPT_LEVEL={opt_level}");
}
