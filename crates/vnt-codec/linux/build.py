#!/usr/bin/env python3
"""Reproducible, GPU-independent build of the replaceable Linux encoder module."""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import urllib.request

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
OUT = ROOT / 'target/linux-codec'
OUT.mkdir(parents=True, exist_ok=True)
lock = (OUT / '.lock').open('w')
fcntl.flock(lock, fcntl.LOCK_EX)
fingerprint = hashlib.sha256(b''.join((HERE / n).read_bytes() for n in ['sources.json', 'encoder.c', 'exports.map', 'build.py'])).hexdigest()
RUNTIME = OUT / 'runtime'
STAMP = OUT / 'complete'
if STAMP.exists() and STAMP.read_text() == fingerprint and (RUNTIME / 'libvnt-linux-encoder.so').exists():
    raise SystemExit(0)
SOURCES = json.loads((HERE / 'sources.json').read_text())
for item in SOURCES:
    archive = OUT / 'downloads' / item['file']
    archive.parent.mkdir(parents=True, exist_ok=True)
    if not archive.exists():
        partial = archive.with_suffix(archive.suffix + '.part')
        with urllib.request.urlopen(item['url'], timeout=60) as response, partial.open('wb') as target:
            shutil.copyfileobj(response, target)
        partial.replace(archive)
    if hashlib.sha256(archive.read_bytes()).hexdigest() != item['sha256']:
        raise RuntimeError(f"Source checksum mismatch: {archive}")
    source = OUT / 'src' / item['directory']
    if not source.exists():
        with tarfile.open(archive) as tar:
            # Validate member paths on Python 3.10 as well as newer Python.
            for member in tar.getmembers():
                if Path(member.name).is_absolute() or '..' in Path(member.name).parts:
                    raise RuntimeError('Unsafe source archive path')
                if member.issym() or member.islnk():
                    if Path(member.linkname).is_absolute() or '..' in Path(member.linkname).parts:
                        raise RuntimeError('Unsafe source archive link')
            tar.extractall(OUT / 'src')
PREFIX = OUT / 'prefix'
env = os.environ.copy()
env['PKG_CONFIG_PATH'] = str(PREFIX / 'lib/pkgconfig') + ':' + env.get('PKG_CONFIG_PATH', '')
# pkgconf must report our private build directory even if LIBRARY_PATH includes it.
env['PKG_CONFIG_ALLOW_SYSTEM_LIBS'] = '1'
JOBS = str(min(os.cpu_count() or 2, 8))
def run(args, cwd=OUT):
    print('+', ' '.join(map(str, args)), flush=True)
    subprocess.run(list(map(str, args)), cwd=cwd, env=env, check=True)
run(['make', 'install', f'PREFIX={PREFIX}'], OUT / 'src/nv-codec-headers-n12.0.16.1')
va_build = OUT / 'libva-build'
if not (va_build / 'build.ninja').exists():
    run(['meson', 'setup', va_build, OUT / 'src/libva-2.20.0', f'--prefix={PREFIX}', '--libdir=lib',
         '--buildtype=release', '-Dwith_x11=no', '-Dwith_glx=no', '-Dwith_wayland=no', '-Dwith_win32=no',
         '-Ddriverdir=/usr/lib/x86_64-linux-gnu/dri'])
run(['ninja', '-C', va_build, '-j', JOBS, 'install'])
ff_build = OUT / 'ffmpeg-build'
ff_build.mkdir(exist_ok=True)
if not (ff_build / 'config.h').exists():
    run([OUT / 'src/ffmpeg-6.1.6/configure', f'--prefix={PREFIX}', '--libdir='+str(PREFIX / 'lib'),
         '--disable-autodetect', '--disable-everything', '--disable-programs', '--disable-doc', '--disable-network',
         '--disable-avformat', '--disable-avdevice', '--disable-avfilter', '--disable-swscale', '--disable-swresample',
         '--disable-postproc', '--disable-shared', '--enable-static', '--enable-pic', '--enable-vaapi', '--enable-libdrm',
         '--enable-ffnvcodec', '--enable-nvenc', '--enable-pthreads', '--disable-xlib',
         '--enable-encoder=h264_nvenc,hevc_nvenc,av1_nvenc,h264_vaapi,hevc_vaapi,av1_vaapi'], ff_build)
configuration = (ff_build / 'config.h').read_text() + (ff_build / 'config_components.h').read_text()
for codec in ['H264_NVENC', 'HEVC_NVENC', 'AV1_NVENC', 'H264_VAAPI', 'HEVC_VAAPI', 'AV1_VAAPI']:
    if '#define CONFIG_' + codec + '_ENCODER 1' not in configuration:
        raise RuntimeError('Required hardware encoder not built: ' + codec)
for option in ['GPL', 'NONFREE']:
    if '#define CONFIG_' + option + ' 0' not in configuration:
        raise RuntimeError('Redistributable LGPL-only configuration required: ' + option)
run(['make', '-j', JOBS], ff_build)
run(['make', 'install'], ff_build)
RUNTIME.mkdir(exist_ok=True)
# Unique SONAMEs plus RTLD_DEEPBIND keep this copy independent of GStreamer's VA libraries.
for base in ['libva', 'libva-drm']:
    source = (PREFIX / 'lib' / (base + '.so.2')).resolve()
    name = base + '-vnt.so.2'
    shutil.copyfile(source, RUNTIME / name)
    run(['patchelf', '--set-soname', name, '--set-rpath', '$ORIGIN', RUNTIME / name])
run(['patchelf', '--replace-needed', 'libva.so.2', 'libva-vnt.so.2', RUNTIME / 'libva-drm-vnt.so.2'])
flags = subprocess.check_output(['pkg-config', '--cflags', 'libdrm'], env=env, text=True).split()
run(['cc', '-shared', '-fPIC', '-O3', '-Wall', '-Wextra', '-Werror', '-I'+str(PREFIX / 'include'), *flags,
     HERE / 'encoder.c', '-o', RUNTIME / 'libvnt-linux-encoder.so',
     PREFIX / 'lib/libavcodec.a', PREFIX / 'lib/libavutil.a',
     '-L'+str(RUNTIME), '-l:libva-drm-vnt.so.2', '-l:libva-vnt.so.2', '-ldrm', '-ldl', '-lm', '-lpthread',
     '-Wl,--exclude-libs,ALL', '-Wl,-z,defs', '-Wl,-rpath,$ORIGIN', '-Wl,--version-script='+str(HERE / 'exports.map')])
licenses = OUT / 'licenses'
licenses.mkdir(exist_ok=True)
for name, source in [('FFmpeg-LGPL.txt', 'ffmpeg-6.1.6/COPYING.LGPLv2.1'),
                     ('libva-COPYING.txt', 'libva-2.20.0/COPYING'),
                     ('nv-codec-headers-LICENSE.txt', 'nv-codec-headers-n12.0.16.1/include/ffnvcodec/nvEncodeAPI.h')]:
    shutil.copyfile(OUT / 'src' / source, licenses / name)
# Ship exact sources and the build recipe alongside binary releases.
source_bundle = OUT / 'vnt-linux-codec-sources.tar.xz'
with tarfile.open(source_bundle, 'w:xz') as tar:
    for item in SOURCES:
        tar.add(OUT / 'downloads' / item['file'], arcname=item['file'])
    for name in ['build.py', 'sources.json', 'encoder.c', 'exports.map']:
        tar.add(HERE / name, arcname='vnt-linux-codec/' + name)
    tar.add(ff_build / 'ffbuild/config.mak', arcname='vnt-linux-codec/ffmpeg-config.mak')
STAMP.write_text(fingerprint)
