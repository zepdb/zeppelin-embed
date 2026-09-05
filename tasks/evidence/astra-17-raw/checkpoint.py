from pathlib import Path
import subprocess,json,time
out=Path(__file__).parent;receipts=[]
for target,name in [
 ('lexical_stats_consistency','astra_16_cached_global_stats_match_exhaustive_live_model'),
 ('lexical_stats_consistency','tombstoned_term_rows_do_not_change_bm25_at_physical_purge'),
 ('hybrid_bounded','astra_10_structured_standalone_and_hybrid_share_exact_lexical_scores'),
 ('store_text_columns','astra_11_phrase_matches_persisted_positions_without_reanalysis'),
 ('store_text_columns','structured_expansions_report_pinned_prefix_fuzzy_and_phonetic_boosts')]:
 cmd=['cargo','test','-p','zeppelin-embed','--test',target,name,'--','--exact'];start=time.time()
 with (out/(name+'.log')).open('x') as log:p=subprocess.run(cmd,stdout=log,stderr=subprocess.STDOUT)
 receipt={'command':cmd,'exit_code':p.returncode,'seconds':time.time()-start};receipts.append(receipt)
 (out/'checkpoint.json').write_text(json.dumps(receipts,indent=2));print(name,p.returncode,flush=True);assert p.returncode==0
