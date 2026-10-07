"""Verify two shipped daemons against a disposable, existing S3 test bucket."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import uuid

parser = argparse.ArgumentParser()
parser.add_argument('--bin-dir', default='target/debug')
args = parser.parse_args()
binaries = Path(args.bin_dir).resolve()
exe = '.exe' if os.name == 'nt' else ''
endpoint = os.environ['AGENTFS_TEST_S3_ENDPOINT']
prefix = 'daemon-tests/' + str(uuid.uuid4())

class Daemon:
    def __init__(self, root):
        self.root = root
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            port = listener.getsockname()[1]
        subprocess.run([str(binaries / ('agentfsd' + exe)), 'init', '--directory', str(root), '--listen', f'127.0.0.1:{port}'], capture_output=True, text=True, check=True)
        config_path = root / 'agentfs.json'
        config = json.loads(config_path.read_text())
        config['remote'] = {'bucket': 'agentfs-integration', 'prefix': prefix, 'region': 'us-east-1', 'endpoint': endpoint, 'allow_http': True}
        config_path.write_text(json.dumps(config))
        self.client = [str(binaries / ('agentfs' + exe)), '--endpoint', f'http://127.0.0.1:{port}/mcp', '--token-file', str(root / 'token')]
        self.log = (root / 'daemon.log').open('w')
        self.process = subprocess.Popen([str(binaries / ('agentfsd' + exe)), 'serve', '--config', str(config_path)], stdout=self.log, stderr=self.log)
        for _ in range(100):
            if self.process.poll() is not None:
                raise AssertionError((root / 'daemon.log').read_text())
            try:
                with socket.create_connection(('127.0.0.1', port), timeout=0.1):
                    return
            except OSError:
                time.sleep(0.05)
        self.stop()
        raise AssertionError('daemon startup timed out')

    def call(self, tool, request, request_id=None):
        command = self.client + ['call', tool, '--json', json.dumps(request)]
        if request_id:
            command += ['--request-id', request_id]
        output = subprocess.run(command, capture_output=True, text=True, timeout=120)
        assert output.returncode == 0, (output.stdout, output.stderr, (self.root / 'daemon.log').read_text())
        return json.loads(output.stdout)

    def stop(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=30)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        self.log.close()

with tempfile.TemporaryDirectory(prefix='agentfs-replica-') as temporary:
    daemons = []
    try:
        for name in ['one', 'two']:
            daemons.append(Daemon(Path(temporary) / name))
        first, second = daemons
        workspace = first.call('workspace_create', {'name': 'replica-integration'}, 'create')['result']['workspace']['id']
        source = first.root / 'exchange' / 'source'
        source.mkdir()
        payload = bytes(range(251)) * 84000
        (source / 'multipart').write_bytes(payload)
        imported = first.call('workspace_import', {'workspace': workspace, 'source': str(source)}, 'import')
        branch = imported['result']['branch']
        revision = branch['formal_head']
        first.call('sync', {'workspace': workspace, 'direction': 'push'}, 'push')
        receipt = first.call('operation_get', {'operation': imported['id']})
        assert receipt['remote_confirmed'], receipt
        second.call('sync', {'workspace': workspace, 'direction': 'pull'}, 'pull')
        destination = second.root / 'exchange' / 'export'
        second.call('workspace_export', {'workspace': workspace, 'revision': revision, 'destination': str(destination)}, 'export')
        assert hashlib.sha256((destination / 'multipart').read_bytes()).digest() == hashlib.sha256(payload).digest()
        status = second.call('workspace_status', {'workspace': workspace})
        current = next(item for item in first.call('workspace_status', {'workspace': workspace})['branches'] if item['id'] == branch['id'])
        guard = {'branch': current['id'], 'authority_epoch': current['authority_epoch'], 'generation': current['generation'], 'expected_head': current['formal_head'], 'mutation_seq': current['mutation_seq']}
        transfer = first.call('ownership_transfer', {'guard': guard, 'target': status['location']}, 'transfer')
        assert transfer['remote_confirmed'], transfer
        second.call('sync', {'workspace': workspace, 'direction': 'pull'}, 'adopt')
        adopted = next(item for item in second.call('workspace_status', {'workspace': workspace})['branches'] if item['id'] == branch['id'])
        assert adopted['owner'] == status['location'] and adopted['state'] == 'writable', adopted
        assert adopted['authority_epoch'] == current['authority_epoch'] + 1
        print('Two-daemon S3 replication, multipart content, remote receipts and ownership transfer passed')
    finally:
        for daemon in reversed(daemons):
            daemon.stop()
