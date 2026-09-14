#!/usr/bin/env python3
"""Export committed desktop icons from the two vector originals with Tauri CLI 2.11.4."""
import argparse
import pathlib
import struct
import subprocess
import tempfile
import xml.etree.ElementTree as ET
import zlib

ROOT = pathlib.Path(__file__).resolve().parents[1]
ICONS = ROOT / 'apps/monhop-desktop/icons'
MARK = ROOT / 'apps/monhop-desktop/ui/assets/monhop-mark.svg'
PNG_SIGNATURE = b'\x89PNG\r\n\x1a\n'


def png_rgba(data):
    if not data.startswith(PNG_SIGNATURE):
        raise ValueError('Not a PNG')
    pos, compressed, header = 8, bytearray(), None
    while pos < len(data):
        length, kind = struct.unpack_from('>I4s', data, pos)
        payload = data[pos + 8:pos + 8 + length]
        if kind == b'IHDR':
            header = struct.unpack('>IIBBBBB', payload)
        if kind == b'IDAT':
            compressed.extend(payload)
        pos += 12 + length
    width, height, depth, color, compression, filtering, interlace = header
    if (depth, color, compression, filtering, interlace) != (8, 6, 0, 0, 0):
        raise ValueError('Expected non-interlaced 8-bit RGBA')
    source = zlib.decompress(compressed)
    stride, rows = width * 4, bytearray()
    if len(source) != (stride + 1) * height:
        raise ValueError('Invalid PNG row length')
    previous = bytes(stride)
    for y in range(height):
        offset = y * (stride + 1)
        mode, row = source[offset], bytearray(source[offset + 1:offset + 1 + stride])
        for x in range(stride):
            left, above = row[x - 4] if x >= 4 else 0, previous[x]
            corner = previous[x - 4] if x >= 4 else 0
            if mode == 4:
                predictor = left + above - corner
                delta = [abs(predictor - value) for value in (left, above, corner)]
                add = (left, above, corner)[delta.index(min(delta))]
            elif mode in (0, 1, 2, 3):
                add = (0, left, above, (left + above) // 2)[mode]
            else:
                raise ValueError('Invalid PNG filter')
            row[x] = (row[x] + add) & 255
        rows.extend(row)
        previous = row
    return width, height, bytes(rows)


def render(svg, sizes, output):
    command = ['cargo', 'tauri', 'icon', str(svg), '--output', str(output)]
    for size in sizes:
        command.extend(['--png', str(size)])
    subprocess.run(command, cwd=ROOT, check=True)
    return {size: (output / f'{size}x{size}.png').read_bytes() for size in sizes}


def small_tile():
    ET.register_namespace('', 'http://www.w3.org/2000/svg')
    mark = ET.parse(MARK).getroot()
    paths = ''.join(ET.tostring(node, encoding='unicode') for node in mark if node.tag.split('}')[-1] != 'title')
    return ('<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64" viewBox="0 0 64 64">'
            '<rect x="4.5" y="4.5" width="55" height="55" rx="13" fill="#151618" stroke="#66686b" stroke-width=".5"/>'
            '<g fill="#f7f8f9" transform="translate(5 4) scale(.84)">' + paths + '</g></svg>')


def ico(frames):
    offset = 6 + 16 * len(frames)
    directory, contents = bytearray(), bytearray()
    for size, data in sorted(frames.items()):
        directory.extend(struct.pack('<BBBBHHII', size % 256, size % 256, 0, 0, 1, 32, len(data), offset))
        contents.extend(data)
        offset += len(data)
    return struct.pack('<HHH', 0, 1, len(frames)) + directory + contents


def icns(frames):
    types = {'icp4': 16, 'icp5': 32, 'icp6': 64, 'ic07': 128, 'ic08': 256, 'ic09': 512,
             'ic10': 1024, 'ic11': 32, 'ic12': 64, 'ic13': 256, 'ic14': 512}
    blocks = b''.join(struct.pack('>4sI', name.encode(), len(frames[size]) + 8) + frames[size] for name, size in types.items())
    return struct.pack('>4sI', b'icns', len(blocks) + 8) + blocks


def generate(check=False):
    version = subprocess.check_output(['cargo', 'tauri', '--version'], text=True).strip()
    if version != 'tauri-cli 2.11.4':
        raise RuntimeError('Use the pinned build-only tauri-cli 2.11.4')
    with tempfile.TemporaryDirectory(prefix='monhop-icons-') as directory:
        temp = pathlib.Path(directory)
        tile = temp / 'small.svg'
        tile.write_text(small_tile())
        frames = render(ICONS / 'icon.svg', [128, 256, 512, 1024], temp / 'full')
        frames.update(render(tile, [16, 24, 32, 48, 64], temp / 'small'))
        mask = render(MARK, [32], temp / 'mask')[32]
        width, height, rgba = png_rgba(mask)
        assert (width, height) == (32, 32)
        exports = {'icon.png': frames[1024], 'icon.ico': ico({s: frames[s] for s in [16, 24, 32, 48, 64, 128, 256]}),
                   'icon.icns': icns(frames), 'tray-mask.bin': rgba[3::4]}
        for name, data in exports.items():
            path = ICONS / name
            if check:
                if not path.exists() or path.read_bytes() != data:
                    raise RuntimeError(f'Stale icon export: {name}')
            else:
                path.write_bytes(data)
    print('MonHop vector exports are current.' if check else 'Generated MonHop app icons and small tray mask.')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true', help='verify exports without changing files')
    generate(parser.parse_args().check)
