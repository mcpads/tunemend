"""Bridge a complete event inventory to an existing Compile score encoder.

The encoder remains externally owned. This adapter supplies independent MIDI
inspection, explicit mapping accounting, module bounds and score readback.
No archive layout, song identity, installation path or game asset is built in.
"""
import collections
import copy
import importlib.util
import json
from pathlib import Path
import struct
import sys
import tempfile


class Refused(ValueError):
    pass


def require(condition, message):
    if not condition:
        raise Refused(message)


def notes_from_events(events):
    held, notes = {}, []
    ports, receiver = {}, None
    for event in sorted(events, key=lambda e: (e['tick'], e['track'], e['index'])):
        m, tick = event['message'], event['tick']
        kind = m['kind']
        if kind == 'port':
            ports[event['track']] = m['port']
        elif kind != 'channel_prefix' and ('channel' in m or kind in ('sysex', 'escape')):
            port = ports.get(event['track'], 0)
            if receiver is None:
                receiver = port
            require(port == receiver,
                    f"{event['id']}: multiple MIDI output ports need a separate mapping")
        if kind not in ('note_on', 'note_off'):
            continue
        key = (m['channel'], m['key'])
        if kind == 'note_on' and m['velocity']:
            require(key not in held, f"{event['id']}: overlapping same-key notes need explicit pairing")
            held[key] = event
        else:
            require(key in held, f"{event['id']}: unpaired note release")
            start = held.pop(key)
            require(tick > start['tick'], f"{event['id']}: nonpositive note duration")
            notes.append((start['tick'], tick, *key, start['message']['velocity'], start['id'], event['id']))
    require(not held, 'unreleased source notes need an explicit ending')
    require(notes, 'the score backend requires at least one paired note')
    return sorted(notes)


def module_memory(data, bank_end):
    memory, regions, i = {}, [], 0
    while True:
        require(i + 2 <= len(data), 'module has no terminator')
        at = int.from_bytes(data[i:i + 2], 'little')
        i += 2
        if at == 0xffff:
            require(i == len(data), 'module has trailing bytes')
            break
        require(i + 2 <= len(data), 'module has no chunk length')
        size = int.from_bytes(data[i:i + 2], 'little')
        i += 2
        require(size > 0 and i + size <= len(data), 'module chunk is truncated or empty')
        end = at + size
        require(0x3a00 <= at < end <= 0x4500 or bank_end <= at < end <= 0xaae0,
                'score chunk intersects a bank, resident samples or unsupported ARAM')
        for address, byte in enumerate(data[i:i + size], at):
            require(address not in memory, 'score chunks overlap')
            memory[address] = byte
        regions.append({'start': at, 'end': end, 'bytes': size})
        i += size
    return memory, regions


def decode_track(memory, start, total, durations, loop, loop_start):
    """Interpret emitted score instructions independently of the encoder."""
    pc, tick, duration = start, 0, None
    calls, loops, stream = [], {}, []
    def byte():
        nonlocal pc
        require(pc in memory, f'score pointer {pc:#x} is outside encoded memory')
        value = memory[pc]
        pc += 1
        return value
    def word():
        return byte() | byte() << 8
    for _ in range(2_000_000):
        op = byte()
        if op <= 0x7f or 0xc0 <= op <= 0xdd:
            following = memory.get(pc)
            if following == 0xde:
                byte()
                duration = byte()
            elif following is not None and 0xdf <= following <= 0xee:
                duration = durations[byte() - 0xdf]
            require(duration is not None and duration > 0, 'score has no positive duration')
            require(tick + duration <= total, f'score crosses the planned ending: tick={tick}, duration={duration}, total={total}, pc={pc:#x}, op={op:#x}')
            stream.append([tick, duration, op])
            tick += duration
        elif op == 0x87:
            stream.append([tick, 0, ['volume', byte()]])
        elif op == 0x9a:
            target = word()
            calls.append(pc)
            require(len(calls) <= 16, 'excessive score call nesting')
            pc = target
        elif op == 0x9b:
            require(calls, 'score return without call')
            pc = calls.pop()
        elif op == 0x8d:
            level, count = byte(), byte()
            require(count > 0 and level not in loops, 'invalid score repeat')
            loops[level] = count
        elif op == 0x81:
            level, target = byte(), word()
            require(level in loops, 'repeat has no counter')
            loops[level] -= 1
            if loops[level]:
                pc = target
            else:
                del loops[level]
        elif op == 0x80:
            target = word()
            if tick == total:
                require(loop and target == loop_start and not calls and not loops,
                        'loop return does not match the intended score start')
                return stream
            pc = target
        elif op == 0x82:
            require(not loop and tick == total and not calls and not loops,
                    'score ends before the planned ending or instead of looping')
            return stream
        else:
            raise Refused(f'unsupported encoded command {op:#x}')
    raise Refused('score interpreter step limit exceeded')


def expected_stream(stream, delay=0):
    out, tick = [], 0
    if delay:
        out.append([0, delay, 0])
        tick = delay
    for length, status in stream:
        if isinstance(status, tuple):
            out.append([tick, 0, list(status)])
            continue
        # Driver notes longer than one byte duration release after the first part.
        while length:
            step = length if length < 256 else (length - 12 if length < 267 else 255)
            out.append([tick, step, status])
            tick += step
            length -= step
            status = 0
    return out


def prepare(request, root):
    files = request['files']
    for name, data in files.items():
        (root / name).write_bytes(bytes(data))
    spec = importlib.util.spec_from_file_location('score_encoder', root / 'encoder.py')
    encoder = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(encoder)
    require((encoder.SCORE_BASE, encoder.HEADER, encoder.BANK_START, encoder.RESIDENT_BRR,
             encoder.TUNING) == (0x3a00, 0x3b5e, 0x4500, 0xaae0, 0x3af0),
            'encoder memory layout does not match this backend; use a compatible profile')
    summary, events = request['summary'], request['events']
    notes = notes_from_events(events)
    division = summary['timing']['ticks_per_quarter']
    mapping = copy.deepcopy(request['mapping'])
    ppqn, offset = mapping['ppqn'], mapping['offset_ticks']
    require(0 < ppqn <= division and division % ppqn == 0,
            'target PPQN must divide source PPQ exactly')
    scale = division // ppqn
    tracks = mapping['tracks']
    require(1 <= len(tracks) <= 8, 'this driver supports one to eight tracks')
    names = [s['name'] for s in tracks]
    require(len(set(names)) == len(names) and all(names), 'track names must be unique and nonempty')
    primary = {s['name']: s for s in tracks if 'copy_of' not in s}
    require(primary, 'at least one non-copy track is required')
    for s in tracks:
        require(all(0 <= c < 16 for c in s['channels']), f"{s['name']}: invalid MIDI channel")
        require(all(0 <= int(k) < 128 and 0 <= v < 30 for k, v in s['drums'].items()),
                f"{s['name']}: invalid percussion mapping")
        require(s.get('quantize') in (None, 'round'), 'quantize must be round or omitted')
        require(s.get('same_tick') in (None, 'last'), 'same_tick must be last or omitted')
        require(-128 <= s['transpose'] <= 127, 'transpose must fit a signed byte')
        require(0 <= s['srcn'] < 55, 'sample index is outside this driver tuning table')
        require(-128 <= s['pan'] <= 128, 'driver pan must be between -128 and 128')
        if 'copy_of' in s:
            require(s['copy_of'] in primary, 'copies must refer to a non-copy track')
            require(s['delay'] % scale == 0 and s['delay'] // scale < 256,
                    'copy delay must be a whole target tick below 256')
        else:
            require(s['channels'], f"{s['name']}: select source channels")
        require(all(t >= offset for t, _ in s['volumes']), 'volume changes precede the source offset')
    require(all(0 <= s < 55 for s in request['tuned_srcns']), 'tuned sample index outside driver table')
    tempos = [e for e in sorted(events, key=lambda e: (e['tick'], e['track'], e['index']))
              if e['message']['kind'] == 'tempo']
    tempo = tempos[0]['message']['microseconds_per_quarter'] if tempos else 500000
    require(tempo > 0, 'tempo must be positive')
    bank = bytes(files['bank.bin'])
    require(len(bank) >= 6 and bank[-2:] == b'\xff\xff', 'bank module is malformed')
    bank_at, bank_size = struct.unpack_from('<HH', bank)
    require(bank_at == 0x4500 and bank_size == len(bank) - 6 and bank_at + bank_size <= 0xaae0,
            'bank must be one bounded module at the declared bank address')
    template = bytes(files['template.bin'])
    require(len(template) >= 4 + 0x15e and struct.unpack_from('<H', template)[0] == 0x3a00,
            'template has no driver prefix tables')
    # Supply generic target assets and canonical inspected notes, bypassing archive
    # extraction and the external encoder's narrower MIDI parser entirely.
    (root / 'modules').mkdir()
    (root / 'modules/template.bin').write_bytes(template)
    (root / 'modules/bank.bin').write_bytes(bank)
    (root / 'sound.json').write_text(json.dumps({'music': [{'module': 0, 'count': 2}],
        'modules': [{'file': 'template.bin'}, {'file': 'bank.bin'}]}))
    encoder.mlk_song = lambda *_: b''
    encoder.midi_notes = lambda _: ([n[:5] for n in notes], division, tempo)
    traced = []
    original_track = encoder.Track
    class TracedTrack(original_track):
        def stream(self, total, fill=0):
            original = copy.deepcopy(self.events)
            selected = {}
            for index, event in sorted(enumerate(self.events), key=lambda item: item[1]):
                selected[event[0]] = (index, event)
            if len(selected) != len(self.events):
                setting = list(primary.values())[len(traced)]
                require(setting.get('same_tick') == 'last',
                        f"{setting['name']}: coincident target onsets; split tracks or explicitly set same_tick=last")
                # Select before serialization. Passing zero-length earlier events
                # to the legacy stream writer can append the same rest twice.
                self.events = [event for _, event in selected.values()]
            stream = super().stream(total, fill)
            traced.append({'events': original, 'selected_indices': [index for index, _ in selected.values()], 'stream': copy.deepcopy(stream)})
            return stream
    encoder.Track = TracedTrack
    declared_tracks = copy.deepcopy(tracks)
    arrangement = dict(mapping, base_request=0, source={'index': 0})
    module, encoding = encoder.encode(arrangement, None, root, request['tuned_srcns'])
    expected_tempo = round((60_000_000 / tempo) * 256 * ppqn / 60 / encoder.FRAME_RATE)
    lead_in_source = min(n[0] for n in notes) - offset if mapping['loop'] and mapping['intro_once'] else 0
    require(lead_in_source >= 0 and lead_in_source % scale == 0, 'intro must be nonnegative whole target ticks')
    lead_in = lead_in_source // scale
    require(encoding.get('lead_in', 0) == lead_in,
            'encoder intro policy differs from mapping.intro_once; select the intended policy with a compatible encoder')
    source_end = max(n[1] for n in notes) - offset - lead_in_source
    unit = scale * ppqn * 4 if mapping['whole_bars'] else scale
    expected_total = -(-source_end // unit) * (ppqn * 4 if mapping['whole_bars'] else 1)
    require(encoding['tempo'] == expected_tempo and 0 < expected_tempo < 256,
            'encoder tempo does not match the selected source tempo and target clock')
    require(encoding['ticks'] == expected_total and expected_total > 0,
            'encoder extent does not match the selected ending')
    memory, regions = module_memory(module, bank_at + bank_size)
    def data(at, length):
        require(all(i in memory for i in range(at, at + length)), 'missing module header/table bytes')
        return bytes(memory[i] for i in range(at, at + length))
    prefix = bytearray(template[4:4 + 0x15e])
    for srcn in request['tuned_srcns']:
        prefix[0xf0 + 2 * srcn:0xf2 + 2 * srcn] = b'\0\0'
    require(data(0x3a10, 0x14e) == prefix[16:], 'encoder changed unrelated prefix data')
    durations = data(0x3a00, 16)
    require(data(0x3b5e, 1)[0] == len(tracks), 'encoded track count mismatch')
    traces = dict(zip(primary, traced))
    require(len(traces) == len(primary), 'encoder did not produce each mapped track')
    headers, starts = {}, {}
    for i, s in enumerate(tracks):
        h = data(0x3b5f + 14 * i, 14)
        headers[s['name']] = h
        starts[s['name']] = int.from_bytes(h[8:10], 'little')
    loop_starts = dict(starts)
    if lead_in:
        for name in primary:
            at = starts[name]
            require(data(at, 1)[0] == 0, 'intro must begin with the selected rest')
            opcode = data(at + 1, 1)[0]
            if opcode == 0xde:
                duration, size = data(at + 2, 1)[0], 3
            else:
                require(0xdf <= opcode <= 0xee, 'intro has no duration')
                duration, size = durations[opcode - 0xdf], 2
            require(duration == lead_in, 'encoded intro duration differs')
            loop_starts[name] += size
    track_rows = []
    for i, s in enumerate(tracks):
        base_name = s.get('copy_of', s['name'])
        delay = s['delay'] // scale if 'copy_of' in s else 0
        h = headers[s['name']]
        declared = declared_tracks[i]
        transpose = declared['transpose']
        if 'copy_of' in declared:
            base = next(t for t in declared_tracks if t['name'] == declared['copy_of'])
            transpose += base['octave_shift'] - declared['octave_shift'] + base['transpose']
        require(-128 <= transpose <= 127, 'adjusted copy transpose exceeds a signed byte')
        expected_header = bytes([i, declared.get('flags', 1), declared['volume'], declared['vol_env'], declared['vibrato'],
            transpose & 255, expected_tempo, i]) + h[8:10] + bytes([declared['srcn'], declared['adsr']]) + struct.pack('<h', declared['pan'])
        require(h == expected_header, f"{s['name']}: header does not match mapping")
        expected = expected_stream(traces[base_name]['stream'], lead_in)
        if delay:
            expected = [[0, delay, 0]] + [[t + delay, length, status] for t, length, status in expected]
        try:
            decoded = decode_track(memory, starts[s['name']], encoding['ticks'] + lead_in + delay,
                                   durations, mapping['loop'], loop_starts[base_name])
        except Refused as error:
            raise Refused(f"{s['name']}: {error}; expected duration={sum(e[1] for e in expected)}") from error
        require(decoded == expected, f"{s['name']}: encoded score does not match mapped events")
        track_rows.append({'name': s['name'], 'settings': declared, 'encoded_transpose': transpose, 'start': starts[s['name']],
                           'copy_of': s.get('copy_of'), 'delay_ticks': delay, 'intro_ticks': lead_in, 'loop_start': loop_starts[base_name],
                           'decoded_events': decoded, 'readback_equal': True})
    changes = collections.Counter()
    rows = {e['id']: {'event': e['id'], 'tick': e['tick'], 'message': e['message'],
                      'targets': [], 'changes': []} for e in events}
    def change(row, kind):
        if kind not in row['changes']:
            row['changes'].append(kind)
            changes[kind] += 1
    by_tuple = {n[:5]: n for n in notes}
    for s, row in zip(tracks, track_rows):
        if 'copy_of' in s:
            changes['track_copy_added'] += 1
            continue
        selected = encoder.track_notes(s, [n[:5] for n in notes])
        require(len(selected) == len(traces[s['name']]['events']), 'encoder selection count mismatch')
        for index, (note, mapped) in enumerate(zip(selected, traces[s['name']]['events'])):
            original = by_tuple[tuple(note)]
            on, off = rows[original[5]], rows[original[6]]
            tick, length, status = mapped
            actual = [e for e in row['decoded_events'] if e[0] == tick + lead_in and e[1] > 0 and e[2] == status] if index in traces[s['name']]['selected_indices'] else []
            target = {'track': s['name'], 'requested': list(mapped), 'encoded': actual}
            on['targets'].append(target)
            off['targets'].append({'track': s['name'], 'paired_on': original[5]})
            change(on, 'instrument_and_mix_mapped')
            change(on, 'velocity_replaced_by_track_volume')
            change(off, 'release_velocity_not_encoded')
            if offset:
                change(on, 'time_origin_shifted')
            if (original[0] - offset) != (tick + lead_in) * scale:
                change(on, 'note_start_quantized')
            if not actual:
                change(on, 'note_removed_by_monophonic_track')
            elif original[1] - original[0] != actual[0][1] * scale:
                change(on, 'note_duration_changed')
            if s['drums']:
                change(on, 'percussion_mapped')
            if s['transpose'] or s['octave_shift']:
                change(on, 'pitch_encoded_with_tuning_mapping')
    for e in events:
        m, row = e['message'], rows[e['id']]
        kind = m['kind']
        if kind in ('note_on', 'note_off'):
            if not row['targets']:
                change(row, 'note_not_selected')
        elif kind == 'tempo':
            change(row, 'tempo_represented_by_driver_rate' if e is tempos[0] else 'tempo_event_not_encoded')
        elif kind == 'end_of_track':
            change(row, 'ending_replaced_by_mapping')
        elif kind == 'controller':
            change(row, f"controller_{m['controller']}_not_encoded")
        elif kind in ('program_change', 'pitch_bend', 'poly_aftertouch', 'channel_aftertouch', 'sysex', 'escape'):
            change(row, kind + '_not_encoded')
        else:
            change(row, 'metadata_not_encoded')
    if not tempos:
        changes['default_tempo_assumed'] += 1
    changes['loop_and_extent_selected'] += 1
    if lead_in:
        changes['intro_played_once'] += 1
    if request['tuned_srcns']:
        changes['sample_tuning_reset'] += len(set(request['tuned_srcns']))
    return {'module': list(module), 'encoding': encoding, 'tracks': track_rows,
            'events': list(rows.values()), 'changes': dict(sorted(changes.items())),
            'verification': {'score_readback': True, 'track_headers': True,
                'prefix_preserved_except_declared_tables': True, 'aram_regions': regions,
                'source_events_accounted': len(rows), 'source_notes_paired': len(notes)},
            'timing': {'source_ppq': division, 'target_ppqn': ppqn, 'source_offset': offset, 'intro_ticks': lead_in,
                       'tempo_us_per_quarter': tempo, 'driver_frames_per_second': encoder.FRAME_RATE},
            'runtime': {'python': sys.version}}


def main():
    request = json.load(sys.stdin)
    try:
        with tempfile.TemporaryDirectory(prefix='tunemend-score-') as temporary:
            result = prepare(request, Path(temporary))
    except (ValueError, KeyError, IndexError, TypeError, AttributeError, struct.error) as error:
        result = {'error': str(error)}
    print(json.dumps(result, separators=(',', ':')))


if __name__ == '__main__':
    main()
