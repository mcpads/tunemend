"""Original minimal encoder test double, not a production driver backend.

Only a single exact-grid tonal track is supported. Its straight-line bytes make
independent expected output and injected-corruption checks easy to inspect.
"""
import struct

SCORE_BASE, HEADER, BANK_START, RESIDENT_BRR, TUNING = 0x3a00, 0x3b5e, 0x4500, 0xaae0, 0x3af0
FRAME_RATE = 60
CORRUPT = False
CORRUPT_LOOP = False


class Track:
    def __init__(self):
        self.events = []

    def stream(self, total, fill=0):
        result, at = [], 0
        for start, length, key in self.events:
            if start > at:
                result.append((start - at, 0))
            result.append((length, key))
            at = start + length
        if at < total:
            result.append((total - at, 0))
        return result


def track_notes(spec, notes):
    return [n for n in notes if n[2] in spec['channels']]


def encode(arrangement, unused, root, tuned_srcns):
    assert len(arrangement['tracks']) == 1 and not tuned_srcns
    spec = arrangement['tracks'][0]
    notes, division, tempo = midi_notes(mlk_song(None, 0))
    scale = division // arrangement['ppqn']
    intro = min(n[0] for n in notes) // scale if arrangement.get('intro_once') and arrangement['loop'] else 0
    total = max(n[1] for n in notes) // scale - intro
    track = Track()
    track.events = [(a // scale - intro, (b - a) // scale, key - 11)
                    for a, b, channel, key, velocity in track_notes(spec, notes)]
    stream = track.stream(total)
    start = HEADER + 15
    code = bytearray([0, 0xde, intro]) if intro else bytearray()
    main_start = start if CORRUPT_LOOP else start + len(code)
    for length, status in stream:
        code += bytes([status, 0xde, length])
    code += bytes([0x80, main_start & 255, main_start >> 8]) if arrangement['loop'] else b'\x82'
    if CORRUPT:
        code[0] += 1
    prefix = (root / 'modules/template.bin').read_bytes()[4:4 + HEADER - SCORE_BASE]
    header = bytes([1, 0, spec.get('flags', 1), spec['volume'], spec['vol_env'], spec['vibrato'],
                    spec['transpose'] & 255, 102, 0, start & 255, start >> 8,
                    spec['srcn'], spec['adsr']]) + struct.pack('<h', spec['pan'])
    body = prefix + header + code
    return struct.pack('<HH', SCORE_BASE, len(body)) + body + b'\xff\xff', {
        'tempo': 102, 'ticks': total, 'lead_in': intro, 'score_bytes': len(body), 'tracks': [{}]}
