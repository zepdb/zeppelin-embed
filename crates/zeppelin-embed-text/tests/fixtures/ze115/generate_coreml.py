#!/usr/bin/env python3
"""Generate an invented two-coordinate model, using only Python's stdlib.

Apple CoreML Model/FeatureTypes/NeuralNetwork.proto schema pinned at
01788ff832317a31a14053a05eab70127b14296d. Model spec4, exact rank2 inputs,
embedding = input_ids + attention_mask. This is a wrapper contract fixture,
not a trained encoder or evidence of accelerator placement.
"""
from pathlib import Path


def varint(n):
    out = bytearray()
    while n > 127:
        out.append((n & 127) | 128)
        n >>= 7
    out.append(n)
    return bytes(out)


def integer(field, n):
    return varint(field << 3) + varint(n)


def blob(field, b):
    return varint((field << 3) | 2) + varint(len(b)) + b


def string(field, s):
    return blob(field, s.encode())


def feature(name, datatype):
    array = blob(1, varint(1) + varint(2)) + integer(2, datatype)
    return string(1, name) + blob(3, blob(5, array))


description = (blob(1, feature('input_ids', 131104))
               + blob(1, feature('attention_mask', 131104))
               + blob(10, feature('embedding', 65568)))
layer = (string(1, 'sum_ids_and_mask') + string(2, 'input_ids')
         + string(2, 'attention_mask') + string(3, 'embedding') + blob(230, b''))
network = blob(1, layer) + integer(5, 1)
model = integer(1, 4) + blob(2, description) + blob(500, network)
Path(__file__).with_name('tiny.mlmodel').write_bytes(model)
