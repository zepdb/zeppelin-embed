#!/usr/bin/env python3
"""Read-only check of retained prerequisites and independent implementation gates."""
import json, pathlib, sys

def edges(data):
    byid = {x['id']: x['key'] for x in data['tickets']}
    return {(byid[x['ticket_id']], byid[x['blocked_by_id']]) for x in data['dependencies']}

def closure(es, root):
    adjacency = {}
    for ticket, blocker in es:
        adjacency.setdefault(ticket, set()).add(blocker)
    done, active = set(), set()
    def visit(node):
        if node in active:
            raise ValueError('cycle at ' + node)
        if node in done:
            return
        active.add(node)
        for blocker in adjacency.get(node, ()):
            visit(blocker)
        active.remove(node)
        done.add(node)
    visit(root)
    return done - {root}

def audit(before, after):
    old, new = edges(before), edges(after)
    required = {('ZE-44','ZE-124'),('ZE-51','ZE-125'),('ZE-53','ZE-127'),('ZE-56','ZE-126'),('ZE-56','ZE-53'),('ZE-68','ZE-128'),('ZE-50','ZE-123'),('ZE-50','ZE-46'),('ZE-52','ZE-40'),('ZE-126','ZE-123')}
    assert not old - new, ('removed original edges', sorted(old-new))
    assert not required - new, ('missing mandatory edges', sorted(required-new))
    for x in after['tickets']:
        closure(new, x['key'])
    before_release, after_release = closure(old, 'ZE-78'), closure(new, 'ZE-78')
    assert before_release <= after_release, ('lost release prerequisites', sorted(before_release-after_release))
    added = {f'ZE-{x}' for x in range(123,129)}
    assert added <= after_release, ('new implementation omitted from release', sorted(added-after_release))
    originals = {f'ZE-{x}' for x in range(32,79)}
    assert originals <= after_release | {'ZE-78'}, ('original implementation omitted', sorted(originals-after_release-{'ZE-78'}))
    for ticket, forbidden in {'ZE-124':{'ZE-43','ZE-44'},'ZE-125':{'ZE-45','ZE-50','ZE-51'},'ZE-126':{'ZE-50','ZE-51','ZE-56'},'ZE-127':{'ZE-50','ZE-51','ZE-52','ZE-53'},'ZE-128':{'ZE-53','ZE-68'}}.items():
        assert not closure(new,ticket)&forbidden, ('false independent gate',ticket)
    return {'passed':True,'original_edges_preserved':len(old),'added_edges':sorted(new-old),'original_release_prerequisites':len(before_release),'current_release_prerequisites':len(after_release),'original_47_implementation_tickets_retained':True,'new_implementation_tickets':sorted(added),'no_cycles':True,'no_false_upstream_integration_blocker_on_independent_code':True,'requirement_mapping':'parallel-contracts.md plus execution-bindings-audit.md and compiler-lowering-audit.md','original_release_closure':sorted(before_release),'current_release_closure':sorted(after_release)}

if __name__ == '__main__':
    before, after = (json.loads(pathlib.Path(x).read_text()) for x in sys.argv[1:3])
    result = audit(before,after)
    print(json.dumps(result,indent=2))
