#!/usr/bin/env python3
"""Package a native release and probe the extracted binary before upload."""
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import tomllib

TARGETS = {
    'aarch64-apple-darwin': ('tunemend', 'gztar'),
    'x86_64-unknown-linux-gnu': ('tunemend', 'gztar'),
    'x86_64-pc-windows-msvc': ('tunemend.exe', 'zip'),
}


def probe(binary, workspace, version):
    actual = subprocess.check_output([str(binary), '--version'], text=True).strip()
    assert actual == f'tunemend {version}', actual
    messages = [
        {'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {
            'protocolVersion': '2025-06-18', 'capabilities': {},
            'clientInfo': {'name': 'release-probe', 'version': '1'}}},
        {'jsonrpc': '2.0', 'method': 'notifications/initialized'},
        {'jsonrpc': '2.0', 'id': 2, 'method': 'tools/list', 'params': {}},
        {'jsonrpc': '2.0', 'id': 3, 'method': 'tools/call', 'params': {
            'name': 'get_capabilities', 'arguments': {}}},
    ]
    # Keep stdin open until every response arrives; EOF can cancel server work.
    from queue import Queue
    from threading import Thread
    process = subprocess.Popen([str(binary), 'mcp', '--workspace', str(workspace)],
                               stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, encoding="utf-8")
    lines = Queue()
    def read():
        for line in process.stdout:
            lines.put(line)
        lines.put(None)
    Thread(target=read, daemon=True).start()
    try:
        process.stdin.write(''.join(json.dumps(m) + '\n' for m in messages))
        process.stdin.flush()
        responses = {}
        while not all(i in responses for i in (1, 2, 3)):
            line = lines.get(timeout=30)
            if line is None:
                raise RuntimeError('MCP server exited before responding')
            message = json.loads(line)
            if 'id' in message:
                responses[message['id']] = message
        assert responses[1]['result']['serverInfo']['version'] == version
        assert responses[1]['result']['instructions'] == Path('src/mcp_instructions.md').read_bytes().decode('utf-8')
        names = {t['name'] for t in responses[2]['result']['tools']}
        assert {'create_project', 'edit_project', 'present_review', 'get_capabilities', 'midi', 'sound'} <= names
        assert responses[3]['result']['structuredContent']['status'] == 'ok'
        requests = [
            ('create_project', {'job_id': 'package-create', 'project': 'package-check',
                'timebase': {'kind': 'effect'}, 'composition': {
                    'voices': [{'id': 'lead'}],
                    'notes': [{'id': 'n1', 'voice': 'lead', 'start': 0,
                               'duration': 100000, 'pitch': 72, 'velocity': 80}],
                    'playback': {'end': 200000}}}),
            ('render_preview', {'job_id': 'package-render', 'project': 'package-check',
                'layout': {'kind': 'full', 'loop_passes': 1}}),
            ('export_project', {'job_id': 'package-export', 'project': 'package-check',
                'destination': 'out/delivery'}),
        ]
        for request_id, (name, arguments) in enumerate(requests, 4):
            process.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': request_id,
                'method': 'tools/call', 'params': {'name': name, 'arguments': arguments}}) + '\n')
            process.stdin.flush()
            while True:
                line = lines.get(timeout=30)
                if line is None:
                    raise RuntimeError('MCP server exited during package workflow')
                response = json.loads(line)
                if response.get('id') == request_id:
                    assert response['result']['structuredContent']['status'] == 'ok', response
                    break

    finally:
        process.stdin.close()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()

    # Reopen storage through the CLI after the MCP process has released it.
    reopened = subprocess.run(
        [str(binary), '--workspace', str(workspace), 'call', 'inspect_project'],
        input=json.dumps({'project': 'package-check', 'view': 'summary'}),
        text=True, encoding='utf-8', capture_output=True, check=True, timeout=30)
    assert json.loads(reopened.stdout)['status'] == 'ok'


def main():
    target = sys.argv[1]
    executable, archive_format = TARGETS[target]
    version = tomllib.loads(Path('Cargo.toml').read_text())['package']['version']
    binary = Path('target') / target / 'release' / executable
    destination = Path('dist').resolve()
    destination.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='tunemend-package-') as temporary:
        root = Path(temporary)
        package = root / 'package'
        package.mkdir()
        shutil.copy2(binary, package / executable)
        for name in ['README.md', 'LICENSE']:
            shutil.copy2(name, package / name)
        shutil.copytree('skills/tunemend', package / 'skills/tunemend')
        archive = shutil.make_archive(str(destination / f'tunemend-{target}'), archive_format, package)
        extracted = root / 'extracted'
        shutil.unpack_archive(archive, extracted)
        probe(extracted / executable, root / 'workspace', version)
        print(f'Packaged and verified {Path(archive).name}')


if __name__ == '__main__':
    main()
