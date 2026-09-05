"""Export timing-only depth/length controls; never replace a production tower."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import time


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('source', type=Path)
    parser.add_argument('output', type=Path)
    parser.add_argument('--python-packages', type=Path, action='append', default=[])
    parser.add_argument('--shapes', default='16,24,32,48,128')
    parser.add_argument('--depths', default='2,3,4')
    args = parser.parse_args()
    args.output.mkdir()
    sys.path[:0] = [str(p) for p in args.python_packages]
    os.environ['HF_HUB_OFFLINE'] = '1'
    os.environ['TRANSFORMERS_OFFLINE'] = '1'
    import torch
    import numpy as np
    import coremltools as ct
    from safetensors.torch import load_file
    torch.set_num_threads(2)
    spec = importlib.util.spec_from_file_location('query_source', args.source/'modeling_arctic_query.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    config = json.loads((args.source/'config.json').read_text())
    config['attention_backend'] = 'eager'
    model = module.ArcticQueryModel(module.ArcticQueryConfig(**config)).eval().float().requires_grad_(False)
    model.load_state_dict(load_file(str(args.source/'model.safetensors')), strict=True)
    layers = list(model.encoder.layer)
    class Wrapper(torch.nn.Module):
        def __init__(self):
            super().__init__()
            self.model = model
        def forward(self, input_ids, attention_mask):
            return self.model.encode_tokens(input_ids, attention_mask)
    wrapper = Wrapper().eval()
    cells = [(int(n),len(layers)) for n in args.shapes.split(',') if n]
    cells += [(32,int(n)) for n in args.depths.split(',') if n]
    receipts = []
    for sequence, depth in cells:
        start = time.monotonic()
        stem = f'clean71-s{sequence}-d{depth}'
        model.encoder.layer = torch.nn.ModuleList(layers[:depth])
        ids = torch.full((1,sequence),config['pad_token_id'],dtype=torch.int32)
        ids[:, :8] = torch.arange(8, dtype=torch.int32)[None,:] + 10
        mask = torch.zeros_like(ids)
        mask[:, :8] = 1
        with torch.inference_mode():
            traced = torch.jit.trace(wrapper,(ids,mask),strict=True)
            reference = wrapper(ids,mask)
            assert torch.equal(reference,traced(ids,mask))
            converted = ct.convert(traced,convert_to='mlprogram',
                inputs=[ct.TensorType(name='input_ids',shape=(1,sequence),dtype=np.int32),
                        ct.TensorType(name='attention_mask',shape=(1,sequence),dtype=np.int32)],
                outputs=[ct.TensorType(name='embedding')], compute_precision=ct.precision.FLOAT16,
                minimum_deployment_target=ct.target.macOS15,skip_model_load=True)
        package=args.output/(stem+'.mlpackage')
        converted.save(str(package))
        command=['xcrun','coremlcompiler','compile',str(package),str(args.output)]
        subprocess.run(command,check=True)
        receipt={'stem':stem,'sequence':sequence,'depth':depth,'timing_only':depth!=len(layers),
                 'command':command,'seconds':time.monotonic()-start,'trace_reference_exact':True}
        receipts.append(receipt)
        (args.output/'exports.json').write_text(json.dumps({'source':str(args.source),
            'weights_sha256':hashlib.sha256((args.source/'model.safetensors').read_bytes()).hexdigest(),
            'torch':torch.__version__,'coremltools':ct.__version__,'exports':receipts},indent=2))
        print('EXPORTED',stem,round(receipt['seconds'],2),flush=True)


if __name__ == '__main__':
    main()
