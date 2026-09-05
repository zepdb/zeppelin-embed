"""Analyze completed budget cells, with exact-result gates before speed claims."""
import argparse
import json
import math
from pathlib import Path
import statistics


def percentile(xs,p):
    return sorted(xs)[math.ceil(len(xs)*p)-1]


def distribution(processes,field):
    xs=[[field(s) for s in d['samples']] for d in processes]
    p50=[percentile(v,.5) for v in xs];p95=[percentile(v,.95) for v in xs]
    return {'p50_ms':statistics.median(p50),'p95_ms':statistics.median(p95),
            'process_p50_ms':p50,'process_p95_ms':p95}


def quality(samples,fixture):
    source={str(d['id']):d['source_id'] for d in fixture['documents']}
    rels={q['id']:{} for q in fixture['queries']}
    for q in fixture['qrels']:rels[q['query-id']][q['corpus-id']]=int(q['score'])
    ndcgs=[];recalls=[]
    for s in samples:
        parents=list(dict.fromkeys(source[h['id']] for h in s['hits']))[:10]
        positive={p:g for p,g in rels[s['query']].items() if g>0}
        ideal=sum((2**g-1)/math.log2(i+2) for i,g in enumerate(sorted(positive.values(),reverse=True)[:10]))
        actual=sum((2**positive.get(p,0)-1)/math.log2(i+2) for i,p in enumerate(parents))
        ndcgs.append(actual/ideal if ideal else 0)
        recalls.append(len(set(parents)&positive.keys())/len(positive) if positive else 0)
    return {'ndcg_at_10':statistics.mean(ndcgs),'recall_at_10':statistics.mean(recalls)}


def main():
    parser=argparse.ArgumentParser();parser.add_argument('root',type=Path);parser.add_argument('fixture',type=Path)
    args=parser.parse_args();root=args.root;fixture=json.loads(args.fixture.read_text());ids=[q['id'] for q in fixture['queries']]
    data={};summary={'api':{},'tower':{}}
    for phase in ['e0','e3']:
        complete=json.loads((root/f'{phase}-runs/complete.json').read_text());assert complete['complete']
        for receipt in json.loads((root/f'{phase}-runs/receipts.json').read_text()):
            assert receipt['exit_code']==0
            label=receipt['label'];d=json.loads((root/label/'results.json').read_text());data[label]=d
            assert [s['query'] for s in d['samples']]==ids,label
            for sample in d['samples']:
                stages=sample.get('stages_ms')
                if stages is not None:
                    assert stages['stage_sum'] <= stages['end_to_end'] + 1e-6,(label,stages)
    def key(d):return [(s['query'],s['hits']) for s in d['samples']]
    reference=data['e0-api-untimed-r1']
    for label,d in data.items():
        if '-api-' in label:assert key(d)==key(reference),label
    for stem in ['e0-api-timed','e0-api-untimed','e3-api-before-timed','e3-api-before-untimed','e3-api-copy-timed','e3-api-copy-untimed']:
        reps=[data[f'{stem}-r{r}'] for r in [1,2,3]]
        row=distribution(reps,lambda s:s['ms'])
        if reps[0]['samples'][0]['stages_ms'] is not None:
            row['stages']={k:distribution(reps,lambda s,k=k:s['stages_ms'][k]) for k in reps[0]['samples'][0]['stages_ms']}
        row.update(quality(reps[0]['samples'],fixture));summary['api'][stem]=row
    for phase in ['e0','e3']:
        for units in ['ane','cpu']:
            stem=f'{phase}-tower-{units}';reps=[data[f'{stem}-r{r}'] for r in [1,2,3]]
            ref=[s['bits'] for s in data[f'e0-tower-{units}-r1']['samples']]
            assert all([s['bits'] for s in d['samples']]==ref for d in reps),stem
            summary['tower'][stem]=distribution(reps,lambda s:s['ms'])
    for clocks in ['timed','untimed']:
        before=summary['api'][f'e3-api-before-{clocks}'];after=summary['api'][f'e3-api-copy-{clocks}']
        after['change']={k:{'absolute_ms':after[k]-before[k],'percent':(after[k]/before[k]-1)*100} for k in ['p50_ms','p95_ms']}
    summary['exact_api_calls']=sum(len(d['samples']) for label,d in data.items() if '-api-' in label)
    summary['exact_tower_vectors']=sum(len(d['samples']) for label,d in data.items() if '-tower-' in label)
    (root/'api-summary.json').write_text(json.dumps(summary,indent=2))
    for label,row in summary['api'].items():print(label,row['p50_ms'],row['p95_ms'],row.get('change'))
    print('exact API calls',summary['exact_api_calls'],'exact tower vectors',summary['exact_tower_vectors'])


if __name__=='__main__':main()
