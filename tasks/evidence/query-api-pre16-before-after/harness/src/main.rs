use serde_json::{Value,json};
use std::{error::Error,fs,path::PathBuf,time::Instant};
use zeppelin_embed_text::{TextStore,QueryOptions,Legs,bundle::Bundle};
fn main()->Result<(),Box<dyn Error>> {
 let args=std::env::args().collect::<Vec<_>>();
 let bundle_path=PathBuf::from(args.get(1).ok_or("bundle")?);
 let fixture:Value=serde_json::from_slice(&fs::read(args.get(2).ok_or("fixture")?)?)?;
 let output=PathBuf::from(args.get(3).ok_or("output")?);fs::create_dir(&output)?;
 let store_path=PathBuf::from(args.get(4).ok_or("store")?);
 let api=args.get(5).ok_or("api")?;
 let legs=match api.as_str(){"dense"=>Legs::Dense,"lexical"=>Legs::Lexical,"hybrid"=>Legs::Hybrid,_=>return Err("invalid api".into())};
 let queries=fixture["queries"].as_array().ok_or("queries")?;
 let bundle=Bundle::open(&bundle_path)?;
 let token_ids=queries.iter().map(|q|bundle.tokenize_query(q["text"].as_str().ok_or("text")?).map(|t|json!({"id":q["id"],"tokens":t.token_ids,"mask":t.attention_mask,"length":t.tokens_per_row})).map_err(Box::<dyn Error>::from)).collect::<Result<Vec<_>,_>>()?;
 let store=TextStore::open(&store_path,&bundle_path,Default::default())?;
 let health=store.health()?;
 if health.segments.iter().map(|s|s.rows).sum::<u64>()!=fixture["expected_chunks"].as_u64().ok_or("chunks")? || health.segments.iter().any(|s|s.tombstones!=0) || health.graph_coverage!=0.0 {return Err(format!("geometry mismatch {health:?}").into());}
 let options=QueryOptions::new(10).with_legs(legs);
 for q in queries.iter().cycle().take(20){store.query_text(q["text"].as_str().ok_or("text")?,options)?;}
 let mut samples=Vec::new();
 for q in queries {
  let start=Instant::now();
  let hits=store.query_text(q["text"].as_str().ok_or("text")?,options)?;
  let ms=start.elapsed().as_secs_f64()*1000.0;
  samples.push(json!({"query":q["id"],"ms":ms,"hits":hits.iter().map(|h|json!({"id":h.doc_id.to_string(),"revision":h.revision,"chunk":h.chunk,"text":h.text,"score":h.score,"score_bits":h.score.to_bits(),"vector_squared_l2_bits":h.vector_squared_l2.map(f64::to_bits),"lexical_bm25_bits":h.lexical_bm25.map(f64::to_bits),"epoch":format!("{:?}",h.epoch)})).collect::<Vec<_>>()}));
 }
 let result=json!({"api":api,"warmups":20,"bundle":bundle_path,"store":store_path,"epoch":format!("{:?}",store.epoch()),"health":format!("{health:?}"),"token_ids":token_ids,"samples":samples});
 fs::write(output.join("results.json"),serde_json::to_vec(&result)?)?;
 store.close()?;
 Ok(())
}
