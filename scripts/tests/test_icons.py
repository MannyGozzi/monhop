import importlib.util
import pathlib
import struct
import unittest
import xml.etree.ElementTree as ET

ROOT = pathlib.Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('generate_icons', ROOT / 'scripts/generate-icons.py')
icons = importlib.util.module_from_spec(spec)
spec.loader.exec_module(icons)


class IconTests(unittest.TestCase):
    def test_vector_sources_are_self_contained_geometry(self):
        for path in [icons.ICONS / 'icon.svg', icons.MARK]:
            root = ET.parse(path).getroot()
            self.assertIn('viewBox', root.attrib)
            self.assertTrue(any(n.tag.endswith('path') for n in root.iter()))
            for node in root.iter():
                self.assertNotIn(node.tag.split('}')[-1], ['image', 'script', 'foreignObject', 'text'])
                for name, value in node.attrib.items():
                    if name.endswith('href'):
                        self.assertTrue(value.startswith('#'))

    def test_large_export_has_transparent_corners_and_opaque_artwork(self):
        width, height, rgba = icons.png_rgba((icons.ICONS / 'icon.png').read_bytes())
        self.assertEqual((width, height), (1024, 1024))
        self.assertEqual(rgba[3], 0)
        self.assertEqual(rgba[-1], 0)
        self.assertEqual(rgba[(512 * width + 512) * 4 + 3], 255)

    def test_windows_icon_contains_optical_sizes_and_high_resolution_frames(self):
        data = (icons.ICONS / 'icon.ico').read_bytes()
        reserved, kind, count = struct.unpack_from('<HHH', data)
        self.assertEqual((reserved, kind, count), (0, 1, 7))
        sizes = []
        end = 6 + count * 16
        for index in range(count):
            width, height, _, _, planes, depth, length, offset = struct.unpack_from('<BBBBHHII', data, 6 + index * 16)
            width, height = width or 256, height or 256
            self.assertEqual((planes, depth, offset), (1, 32, end))
            self.assertEqual(icons.png_rgba(data[offset:offset + length])[:2], (width, height))
            sizes.append(width)
            end += length
        self.assertEqual(sizes, [16, 24, 32, 48, 64, 128, 256])
        self.assertEqual(end, len(data))

    def test_macos_icon_contains_standard_and_retina_sizes(self):
        data = (icons.ICONS / 'icon.icns').read_bytes()
        self.assertEqual(struct.unpack_from('>4sI', data), (b'icns', len(data)))
        offset, sizes = 8, {}
        while offset < len(data):
            name, length = struct.unpack_from('>4sI', data, offset)
            self.assertGreater(length, 8)
            sizes[name.decode()] = icons.png_rgba(data[offset + 8:offset + length])[:2]
            offset += length
        self.assertEqual(offset, len(data))
        self.assertEqual(sizes['icp4'], (16, 16))
        self.assertEqual(sizes['ic11'], (32, 32))
        self.assertEqual(sizes['ic12'], (64, 64))
        self.assertEqual(sizes['ic10'], (1024, 1024))

    def test_small_mask_keeps_seven_separate_shapes_and_smooth_edges(self):
        mask = (icons.ICONS / 'tray-mask.bin').read_bytes()
        self.assertEqual(len(mask), 32 * 32)
        self.assertTrue(any(0 < alpha < 255 for alpha in mask))
        self.assertEqual(mask[:32], bytes(32))
        self.assertEqual(mask[-32:], bytes(32))
        occupied = {i for i, value in enumerate(mask) if value >= 128}
        components = 0
        while occupied:
            pending = [occupied.pop()]
            components += 1
            while pending:
                i = pending.pop()
                for dx, dy in [(1, 0), (-1, 0), (0, 1), (0, -1)]:
                    x, y = i % 32 + dx, i // 32 + dy
                    neighbor = y * 32 + x
                    if 0 <= x < 32 and 0 <= y < 32 and neighbor in occupied:
                        occupied.remove(neighbor)
                        pending.append(neighbor)
        self.assertEqual(components, 7)


if __name__ == '__main__':
    unittest.main()
