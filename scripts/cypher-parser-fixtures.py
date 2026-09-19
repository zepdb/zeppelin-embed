#!/usr/bin/env python3
"""Check/regenerate syntax-only excerpts; never runs or certifies the TCK."""
import argparse
import pathlib
import re
import subprocess
import hashlib
import json

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("source", type=pathlib.Path, help="local pinned openCypher checkout")
parser.add_argument("--write", action="store_true", help="regenerate the syntax fixture")
args = parser.parse_args()
root = args.source
repo = pathlib.Path(__file__).resolve().parent.parent
pin='007895aff5f33097d67b2e48a0a2babd6bd18590'
assert subprocess.check_output(['git','-C',str(root),'rev-parse','HEAD'],text=True).strip()==pin
selected={
'clauses/match/Match1':range(1,7),'clauses/match/Match2':range(1,9),'clauses/match/Match3':[17,18,23,29],'clauses/match/Match4':[1,3,6],'clauses/match/Match7':[1,8,10,24],
'clauses/match-where/MatchWhere6':[2,4],'clauses/with/With6':[1,2,3],
'expressions/aggregation/Aggregation1':[1,2],'expressions/aggregation/Aggregation5':[1,2],'expressions/aggregation/Aggregation8':[1,2],'clauses/return/Return5':[2],
'expressions/boolean/Boolean1':[1],'expressions/boolean/Boolean2':[1],'expressions/boolean/Boolean3':[1],'expressions/boolean/Boolean4':[1],
'expressions/null/Null1':[1,2,3,4,6],'expressions/list/List1':[1,2,3,4],'expressions/list/List3':range(1,8),
'clauses/create/Create1':range(1,13),'clauses/set/Set2':[1,2,3],'clauses/set/Set3':range(1,9),'clauses/set/Set6':[5],'clauses/delete/Delete1':range(1,8),
'clauses/remove/Remove1':[1,3,5,6],'clauses/remove/Remove2':range(1,6),'clauses/remove/Remove3':[1,15]}
rows=[]; metadata=[]; n=0
for key,numbers in selected.items():
 path='tck/features/'+key+'.feature'; raw=(root/path).read_bytes()
 assert raw==subprocess.check_output(['git','-C',str(root),'show',pin+':'+path])
 text=raw.decode(); scenarios=re.split(r'(?m)^  Scenario(?: Outline)?: ',text)[1:]
 for scenario in scenarios:
  num=int(re.match(r'\[(\d+)\]',scenario)[1])
  if num not in numbers:continue
  n+=1
  for idx,m in enumerate(re.finditer(r'(And having executed|When executing query):\n\s+"""\n(.*?)\n\s+"""',scenario,re.S)):
   stage='setup' if m[1].startswith('And') else 'query'
   query=m[2]
   reject=stage=='query' and 'InvalidParameterUse' in scenario
   label=f'{key}.feature [{num}] {stage}'
   rows.append(('reject' if reject else 'parse')+' '+label+'\n'+query)
   metadata.append({'scenario':label,'sha256':hashlib.sha256(query.encode()).hexdigest(),'parser_expectation':'reject' if reject else 'parse','source_sha256':hashlib.sha256(raw).hexdigest()})
assert n==99,(n,len(rows))
notice='''# Syntax-only excerpts of 99 selected original openCypher TCK scenarios.
# Source: https://github.com/opencypher/openCypher at 007895aff5f33097d67b2e48a0a2babd6bd18590.
# This fixture executes ONLY the lexer/parser; it proves no original TCK result,
# semantic error mapping, state mutation, or conformance expectation.
# Two InvalidParameterUse cases reject here; RelationshipUniquenessViolation and
# DeleteConnectedNode cases still require binder/executor validation in ZE-55/59.
# Copyright (c) 2015-2023 "Neo Technology," Network Engine for Objects in Lund AB.
# Licensed under the Apache License, Version 2.0. See OPENCYPHER-LICENSE-APACHE.
# This work was created by the collective efforts of the openCypher community.
# Cypher is a registered trademark of Neo4j Inc. These implementation extensions
# are not approved by the public consensus process of the openCypher Implementers Group.
'''
full_notice = (root / "tck/features/clauses/with/With6.feature").read_text().split("#encoding:")[0]
fixture = full_notice + notice + '\n---\n' + '\n---\n'.join(rows) + '\n'
target = repo / "crates/zeppelin-embed-cypher/tests/fixtures/selected-tck-syntax.txt"
if args.write:
    target.write_text(fixture)
else:
    assert target.read_text() == fixture, "syntax fixture drift"
print(json.dumps(metadata, indent=2))
print(n,'selected scenarios;',len(rows),'syntax inputs;',sum(m['parser_expectation']=='reject' for m in metadata),'syntax rejections')
print([(m['scenario'],m['parser_expectation']) for m in metadata if m['parser_expectation']=='reject'])
