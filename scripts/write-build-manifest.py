#!/usr/bin/env python3
"""Fingerprint source inputs and collect only runtime outputs used by this Cargo build."""
import argparse
import hashlib
import json
import pathlib
import shutil
import subprocess

ROOT = pathlib.Path(__file__).resolve().parent.parent


def digest(path):
    value = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            value.update(block)
    return value.hexdigest()


def source_id():
    paths = [ROOT / 'Cargo.toml', ROOT / 'Cargo.lock']
    for crate in ('ale-core', 'ale-cli', 'ale-gui', 'ale-modeld'):
        paths += [ROOT / crate / 'Cargo.toml', ROOT / crate / 'build.rs']
        paths += list((ROOT / crate / 'src').rglob('*.rs'))
        paths += list((ROOT / crate / 'tests').rglob('*.rs'))
    for directory in ('ale-gui/ui', 'assets', 'scripts', '.cargo', 'vendor/i-slint-backend-winit-1.16.1'):
        paths += [path for path in (ROOT / directory).rglob('*')
                  if path.is_file() and '__pycache__' not in path.parts and '.git' not in path.parts]
    value = hashlib.sha256()
    for path in sorted(set(paths)):
        if path.is_file():
            value.update(path.relative_to(ROOT).as_posix().encode() + b'\0')
            value.update(bytes.fromhex(digest(path)))
    return value.hexdigest()


def copy_runtimes(log, package):
    # Cached build-script messages also identify the output directories selected
    # by this build. Never scan all stale Cargo build folders for matching DLLs.
    candidates = {}
    for line in log.read_text(encoding='utf-8').splitlines():
        message = json.loads(line)
        if message.get('reason') != 'build-script-executed':
            continue
        if not any(name in message.get('package_id', '') for name in ('sherpa', 'ort-sys')):
            continue
        for path in pathlib.Path(message['out_dir']).rglob('*.dll'):
            fingerprint = digest(path)
            previous = candidates.get(path.name.lower())
            if previous and previous[0] != fingerprint:
                raise SystemExit(f'Ambiguous runtime dependency: {path.name}')
            candidates[path.name.lower()] = (fingerprint, path)
    for _, path in candidates.values():
        shutil.copy2(path, package / path.name)
    return {path.name: fingerprint for fingerprint, path in candidates.values()}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--source-id', action='store_true')
    parser.add_argument('--package', type=pathlib.Path)
    parser.add_argument('--target')
    parser.add_argument('--expected-source')
    parser.add_argument('--build-log', type=pathlib.Path)
    args = parser.parse_args()
    fingerprint = source_id()
    if args.source_id:
        print(fingerprint)
        return
    if not args.package or not args.target or not args.expected_source:
        parser.error('--package, --target and --expected-source are required')
    if args.expected_source != fingerprint:
        raise SystemExit('Source inputs changed during packaging; rebuild before delivery')
    runtimes = {}
    if args.target.endswith('windows-msvc'):
        if not args.build_log:
            parser.error('MSVC runtime collection requires --build-log')
        runtimes = copy_runtimes(args.build_log, args.package)
    files = {path.name: {'bytes': path.stat().st_size, 'sha256': digest(path)}
             for path in sorted(args.package.iterdir()) if path.suffix.lower() in ('.exe', '.dll', '.pdb')}
    manifest = {
        'schema_version': 2, 'target': args.target, 'source_sha256': fingerprint,
        'build_id': fingerprint,
        'git_commit': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
        'uncommitted_source': bool(subprocess.check_output(['git', 'status', '--porcelain'], cwd=ROOT, text=True).strip()),
        'rustc': subprocess.check_output(['rustc', '-vV'], text=True).strip(),
        'local_asr_compiled': args.target.endswith('windows-msvc'),
        'native_acceptance': 'pending', 'runtime_dlls': runtimes, 'files': files,
    }
    (args.package / 'build-manifest.json').write_text(json.dumps(manifest, indent=2) + '\n', encoding='utf-8')


if __name__ == '__main__':
    main()
