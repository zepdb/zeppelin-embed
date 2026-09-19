#[path = "/tmp/ze-125-review-5/tests/adversarial-oracle/src/graph_relational.rs"]
mod model;
fn main() {
 let negative=(-3_i64) as u128;
 assert!(model::check(0,&[-3,1,-3],&[],&[negative,negative,1]).is_ok());
 assert!(model::check(0,&[-3,1,-3],&[],&[1,negative,negative]).is_err());
 assert!(model::check(0,&[i64::MAX,0,i64::MIN],&[],&[i64::MIN as u128,0,i64::MAX as u128]).is_ok());
 assert!(model::check(1,&[-3,1,-3],&[],&[negative,1]).is_ok());
 assert!(model::check(2,&[],&[(1u128<<100)+7,7,7],&[7,(1u128<<100)+7]).is_ok());
 assert!(model::check_failure(false,1,0).is_ok());
 assert!(model::check_failure(false,1,1).is_err());
}
