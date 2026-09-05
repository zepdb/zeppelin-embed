import argparse
import json
import math
from pathlib import Path
import statistics
from analyze import distribution


def cosine(a,b):
    return sum(x*y for x,y in zip(a,b))/math.sqrt(sum(x*x for x in a)*sum(y*y for y in b))


def main():
    parser=argparse.ArgumentParser();parser.add_argument('root',type=Path);args=parser.parse_args();root=args.root
    assert json.loads((root/'curve-runs/complete.json').read_text())['complete']
    receipts=json.loads((root/'curve-runs/receipts.json').read_text());assert len(receipts)==54
    cells={}
    for receipt in receipts:
        assert receipt['exit_code']==0
        d=json.loads((root/receipt['label']/'results.json').read_text())
        cells[receipt['shape'],receipt['units'],receipt['rep']]=d
    common=[s['query'] for s in cells['s64-d6','ane',1]['samples']]
    for d in cells.values():assert [s['query'] for s in d['samples']]==common
    reference={s['query']:s['embedding'] for s in cells['s64-d6','ane',1]['samples']}
    report={'queries':len(common),'query_ids':common,'max_nonpadding_tokens':16,'models':{}}
    for shape in dict.fromkeys(r['shape'] for r in receipts):
        row={}
        for units in ['ane','cpu']:
            ds=[cells[shape,units,r] for r in [1,2,3]]
            assert all([s['bits'] for s in d['samples']]==[s['bits'] for s in ds[0]['samples']] for d in ds[1:]),(shape,units)
            row[units]=distribution(ds,lambda s:s['ms'])
        sims=[cosine(s['embedding'],reference[s['query']]) for s in cells[shape,'ane',1]['samples']]
        row['cosine_to_s64d6']={'minimum':min(sims),'mean':statistics.mean(sims)}
        row['cpu_over_ane_p50_ratio']=row['cpu']['p50_ms']/row['ane']['p50_ms']
        row['runtime_acceleration_witness_gt2x']=row['cpu_over_ane_p50_ratio']>2
        report['models'][shape]=row
    (root/'curve-summary.json').write_text(json.dumps(report,indent=2))
    print('matched queries',len(common),'processes',len(receipts))
    for name,row in report['models'].items():
        print(name,'ANE',row['ane']['p50_ms'],row['ane']['p95_ms'],'CPU',row['cpu']['p50_ms'],'ratio',row['cpu_over_ane_p50_ratio'],'cos',row['cosine_to_s64d6'])


if __name__=='__main__':main()
