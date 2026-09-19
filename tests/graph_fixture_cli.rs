//! Offline input generation only; this does not run a product qualification.
#[path = "tooling_seed.rs"]
mod test_support;
use rand::Rng;
use zeppelin_embed_bench::graph_fixture::{self, Config, Scale, WordStream};
fn main() {
    if let Err(error) = run() {
        eprintln!("graph-fixture: {error}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), String> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.as_slice(){
  [command,path] if command=="validate"=>{println!("{:?}",graph_fixture::validate_fixture(std::path::Path::new(path))?);Ok(())},
  [command,scale,path,pin] if command=="generate"=>{
   let scale=match scale.as_str(){"baseline"=>Scale::Baseline,"stress"=>Scale::Stress,"small"=>Scale::Small,_=>return Err("scale must be baseline, stress or small".into())};let config=Config::new(scale);
   let mut factory=|name:&str|{let mut rng=test_support::seeded_rng(name,config.seed);Box::new(move||rng.random()) as WordStream};
   println!("{:?}",graph_fixture::write_fixture(std::path::Path::new(path),config,pin,&mut factory)?);Ok(())
  },
  _=>Err("usage: graph-fixture generate baseline|stress|small OUTPUT SOURCE_PIN; graph-fixture validate OUTPUT".into()),
 }
}
