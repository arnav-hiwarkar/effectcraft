import argparse
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import time

parser = argparse.ArgumentParser()
parser.add_argument('--full-tests', action='store_true')
parser.add_argument('--skip-build', action='store_true')
parser.add_argument('--cli')
parser.add_argument('--output', required=True)
args = parser.parse_args()
out = Path(args.output).resolve()
out.mkdir(parents=True, exist_ok=True)
env = os.environ.copy()
env['EFFECTCRAFT_CONFIG_DIR'] = str(out / 'settings')
env['EFFECTCRAFT_AEP_FIXTURES'] = str(Path('corpus/aep').resolve())
env['EFFECTCRAFT_AEP_FIXTURES_REQUIRED'] = '1'
results = []


def run(name, command, expected=0, timeout=7200):
    started = time.monotonic()
    print(f'START {name}', flush=True)
    path = out / (name + '.log')
    with path.open('w') as log:
        try:
            code = subprocess.run(command, env=env, stdout=log, stderr=subprocess.STDOUT, timeout=timeout).returncode
        except subprocess.TimeoutExpired:
            code = 124
    text = path.read_text(errors='replace')
    status = code == expected if expected is not None else code > 0 and code != 124
    result = {'name': name, 'command': command, 'exit': code, 'passed': status,
              'seconds': round(time.monotonic() - started, 3),
              'test_results': re.findall(r'test result:.*', text),
              'skip_lines': [line for line in text.splitlines() if re.search(r'SKIPPED|skipping|no adapter', line, re.I)]}
    results.append(result)
    (out / 'results.json').write_text(json.dumps({'platform': platform.platform(), 'results': results}, indent=2))
    print(f'END {name} exit={code} pass={status}', flush=True)
    return status


run('revision', ['git', 'rev-parse', 'HEAD'])
run('toolchain', ['rustc', '--version', '--verbose'])
if not args.skip_build:
    corpus = run('aep-corpus', ['cargo', 'run', '--locked', '-p', 'xtask', '--', 'aep-corpus'])
    if corpus:
        if args.full_tests:
            run('workspace-tests', ['cargo', 'test', '--locked', '--workspace', '--lib', '--bins', '--tests', '--no-fail-fast', '--', '--nocapture'])
            run('doctests', ['cargo', 'test', '--locked', '--workspace', '--doc'])
        else:
            run('mandatory-aep', ['cargo', 'test', '--locked', '-p', 'effectcraft-aep', '--lib', '--tests', '--no-fail-fast', '--', '--nocapture'])
            run('aep-integration', ['cargo', 'test', '--locked', '-p', 'effectcraft-engine', '-p', 'effectcraft-ui-egui', '--lib', '--tests', '--no-fail-fast', 'aep', '--', '--nocapture'])
    run('release-build', ['cargo', 'build', '--locked', '--release', '-p', 'effectcraft', '-p', 'effectcraft-cli'])

suffix = '.exe' if os.name == 'nt' else ''
cli = str(Path(args.cli or ('target/release/effectcraft-cli' + suffix)).resolve())
if Path(cli).is_file():
    scene = out / 'scene.ecproj'
    commands = [
        {'command': 'comp.new', 'params': {'name': 'Audit scene', 'width': 128, 'height': 72, 'frameRate': 24, 'duration': 1}},
        {'command': 'layer.newSolid', 'params': {'name': 'Background', 'color': '#204060', 'width': 128, 'height': 72}},
        {'command': 'layer.newSolid', 'params': {'name': 'Foreground', 'color': '#ff8000', 'width': 60, 'height': 40}},
        {'command': 'prop.set', 'params': {'layer': '#1', 'path': 'transform/opacity', 'value': 60}},
        {'command': 'mask.new', 'params': {'layer': '#1', 'vertices': [[0, 0], [60, 0], [30, 40]], 'closed': True}},
    ]
    steps = out / 'commands.json'
    steps.write_text(json.dumps(commands))
    if run('create-save', [cli, 'run', '--commands', str(steps), '--empty', '--save-as', str(scene), '--json'], timeout=120):
        run('reopen', [cli, 'info', '--project', str(scene), '--json'], timeout=120)
        for gpu in [False, True]:
            flags = ['--gpu'] if gpu else []
            label = 'gpu' if gpu else 'cpu'
            run('render-' + label, [cli, 'render-frame', '--project', str(scene), '--comp', 'Audit scene', '--out', str(out / (label + '.png')), '--json'] + flags, timeout=120)
        for fmt, extension in [('h264', 'mp4'), ('prores', 'mov'), ('exr', 'exr'), ('gif', 'gif')]:
            dest = out / ('export-' + fmt + '.' + extension)
            run('export-' + fmt, [cli, 'render', '--project', str(scene), '--comp', 'Audit scene', '--format', fmt, '--end', '0.125', '--out', str(dest), '--json'], timeout=180)
            if extension in ('mp4', 'mov', 'gif') and shutil.which('ffprobe') and dest.exists():
                run('probe-' + fmt, ['ffprobe', '-v', 'error', '-count_frames', '-show_streams', '-of', 'json', str(dest)], timeout=60)
        aep = out / 'scene.aep'
        if run('export-aep', [cli, 'exec', 'file.exportAep', '--project', str(scene), '--params', json.dumps({'path': str(aep)}), '--json'], timeout=120):
            run('import-aep', [cli, 'exec', 'file.importAep', '--empty', '--params', json.dumps({'path': str(aep)}), '--save-as', str(out / 'aep-roundtrip.ecproj'), '--json'], timeout=120)
        run('benchmark', [cli, 'bench', '--project', str(scene), '--n', '5', '--play', '4'], timeout=120)
    broken = out / 'malformed.ecproj'
    broken.write_bytes(b'{not a project')
    run('malformed-project', [cli, 'info', '--project', str(broken), '--json'], expected=None, timeout=30)
else:
    results.append({'name': 'cli-smokes', 'passed': False, 'reason': 'No release CLI was built'})
(out / 'results.json').write_text(json.dumps({'platform': platform.platform(), 'results': results}, indent=2))
sys.exit(0 if all(r['passed'] for r in results) else 1)
