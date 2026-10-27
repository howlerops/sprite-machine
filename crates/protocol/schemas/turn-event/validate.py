#!/usr/bin/env python3
"""Validate every example against envelope.schema.json (SPEC-001). Run from the repo root."""
import json, glob, sys, os
from jsonschema import Draft202012Validator
from referencing import Registry, Resource
R = os.path.dirname(os.path.abspath(__file__))
reg = Registry()
for f in glob.glob(f'{R}/*.schema.json') + glob.glob(f'{R}/events/*.schema.json'):
    d = json.load(open(f)); Draft202012Validator.check_schema(d)
    reg = reg.with_resource(d['$id'], Resource.from_contents(d))
env = json.load(open(f'{R}/envelope.schema.json'))
v = Draft202012Validator(env, registry=reg, format_checker=Draft202012Validator.FORMAT_CHECKER)
bad = 0; seen = set()
for f in sorted(glob.glob(f'{R}/examples/*.json')):
    ex = json.load(open(f)); errs = list(v.iter_errors(ex)); seen.add(ex['event']['type'])
    if errs: bad += 1; print('INVALID', f, [e.message[:200] for e in errs][:3])
names = {os.path.basename(f).split('.')[0] for f in glob.glob(f'{R}/events/*.schema.json')}
print('missing examples:', names - seen)
# negative checks
neg = json.load(open(f'{R}/examples/Activity.json')); neg['event']['content'] = {'path': 'x'}
assert list(v.iter_errors(neg)), 'diff without new_text must fail'
neg = json.load(open(f'{R}/examples/TextMessageContent.json')); neg['event']['delta'] = ''
assert list(v.iter_errors(neg)), 'empty delta must fail'
neg = json.load(open(f'{R}/examples/RunStarted.json')); neg['event']['type'] = 'Nope'
assert list(v.iter_errors(neg)), 'unknown type must fail'
print('examples', len(glob.glob(f'{R}/examples/*.json')), 'bad', bad); sys.exit(1 if bad else 0)
